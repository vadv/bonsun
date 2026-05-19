//! Подготовка bundle'а на хосте и заливка в контейнер.
//!
//! BDD-сценарий описывает bundle через docstring-блоки:
//! ```
//! Given a bundle with manifest:
//!   """
//!   apt.package(name = "nginx")
//!   """
//! ```
//! Helper'ы материализуют bundle во временной директории, дописывают
//! `bundle.toml`, копируют всё в контейнер.
//!
//! Bundle rev 2 убрал `--inventory`. BDD-сценарии, использующие
//! `Given a bundle with inventory:`, продолжают работать: helper кладёт
//! yaml в `inventory/legacy.yaml` и оборачивает manifest следующим
//! образом — в начало добавляется:
//! ```
//! load("@bosun/builtins", "inventory")
//! inv = inventory.read("inventory/legacy.yaml")
//! ```
//! Это даёт легаси-bundle'ам глобал `inv`, к которому шаблоны обращаются
//! через template(inv = inv).

use std::fs;
use std::path::Path;

use cucumber::{gherkin::Step, given, when};
use tempfile::TempDir;

use crate::docker_helper::{docker_cp_into, docker_exec_shell};
use crate::world::BosunWorld;

const DEFAULT_BUNDLE_TOML: &str = r#"[bundle]
name = "bdd-bundle"
version = "0.1.0"
requires_bosun = ">=0.1, <1.0"
entry = "manifests/main.star"

[bundle.inventory]
default_merge_strategy = "deep_map_replace_list"

[bundle.tags]
bdd = "BDD scenarios"
"#;

fn write_file(path: &Path, body: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, body)?;
    Ok(())
}

/// Сгенерировать manifest, оборачивая user-body в нужные `load()`-преамбулы.
///
/// Логика:
/// - Если user-body уже содержит `load(`, оставляем как есть.
/// - Иначе подмешиваем стандартный `load("@bosun/builtins", "apt", "file",
///   "template", "inventory")`.
/// - Если задан inventory_yaml, добавляем `inv = inventory.read(...)` и
///   декорируем все `template("X")` → `template("X", inv = inv)` через
///   regex.
fn assemble_manifest(user_body: &str, has_inventory: bool) -> String {
    let mut out = String::new();
    if !user_body.contains("load(") {
        out.push_str(
            "load(\"@bosun/builtins\", \"apt\", \"file\", \"template\", \"inventory\", \"tags\")\n",
        );
    }
    if has_inventory && !user_body.contains("inventory.read(") {
        out.push_str("inv = inventory.read(\"inventory/legacy.yaml\")\n");
    }
    // Декорируем template("foo.j2") → template("foo.j2", inv = inv), если
    // user-body использует один из этих вызовов и не передаёт inv явно.
    // Простой regex: ищем `template("...")` без kwargs.
    let decorated = if has_inventory {
        decorate_template_calls(user_body)
    } else {
        user_body.to_string()
    };
    out.push_str(&decorated);
    out
}

fn decorate_template_calls(body: &str) -> String {
    // Простая state-machine: ищем `template("..."` и `)`, если между ними
    // нет `,` (значит — без kwargs), вставляем `, inv = inv` перед `)`.
    let mut out = String::with_capacity(body.len() + 32);
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 9 <= bytes.len() && &bytes[i..i + 9] == b"template(" {
            // Найти закрывающий ")" на уровне 0; собрать содержимое аргументов.
            let mut depth = 1;
            let mut j = i + 9;
            let mut content_start = j;
            while j < bytes.len() && depth > 0 {
                match bytes[j] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                if depth > 0 {
                    j += 1;
                }
            }
            if depth == 0 && j > content_start {
                let inner = &body[content_start..j];
                let has_kwargs = inner.contains(',') || inner.contains('=');
                out.push_str("template(");
                out.push_str(inner);
                if !has_kwargs {
                    out.push_str(", inv = inv");
                }
                out.push(')');
                i = j + 1;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

pub fn materialize_and_upload_bundle(world: &mut BosunWorld) -> anyhow::Result<String> {
    let id = world
        .container_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no container is running"))?;

    let tmp = TempDir::new()?;
    let root = tmp.path().join("bundle");
    fs::create_dir_all(&root)?;

    let bundle_toml = world
        .bundle_toml
        .clone()
        .unwrap_or_else(|| DEFAULT_BUNDLE_TOML.to_string());
    write_file(&root.join("bundle.toml"), &bundle_toml)?;

    let user_body = world.manifest_body.clone().unwrap_or_default();
    let has_inventory = world.inventory_yaml.is_some();
    let manifest = assemble_manifest(&user_body, has_inventory);
    write_file(&root.join("manifests/main.star"), &manifest)?;

    if let Some(inv_body) = &world.inventory_yaml {
        write_file(&root.join("inventory/legacy.yaml"), inv_body)?;
    }

    // Templates: новая раскладка — `roles/legacy/templates/X` плюс
    // обёртка-роль, которая вызывает template'ы. Чтобы не ломать существующие
    // сценарии с прямым вызовом template() из main.star, мы кладём шаблоны
    // в _lib/legacy/templates/ и в main.star тоже создаём load на @lib/legacy
    // (если шаблон используется). Однако в текущих BDD-сценариях template()
    // вызывается напрямую из main.star, что новый template() rejects через
    // TemplateFromManifests. Поэтому если шаблоны есть, оборачиваем в роль.
    if !world.templates.is_empty() {
        // Делаем «legacy» роль и переписываем main.star: он load'ит роль и
        // вызывает её функцию, которая внутри зовёт template().
        let role_body = build_legacy_role_module(&user_body, has_inventory);
        write_file(&root.join("roles/legacy/main.star"), &role_body)?;
        for (rel, body) in &world.templates {
            write_file(&root.join("roles/legacy/templates").join(rel), body)?;
        }
        // Перезаписываем main.star, чтобы он звал legacy роль.
        let main = if has_inventory {
            "load(\"@bosun/builtins\", \"inventory\")\nload(\"@roles/legacy\", \"main\")\ninv = inventory.read(\"inventory/legacy.yaml\")\nmain(inv = inv)\n"
        } else {
            "load(\"@roles/legacy\", \"main\")\nmain()\n"
        };
        write_file(&root.join("manifests/main.star"), main)?;
    }

    let res = docker_exec_shell(&id, "rm -rf /work/bundle && mkdir -p /work")?;
    if res.exit_code != 0 {
        anyhow::bail!("failed to clear /work/bundle: {}", res.stderr);
    }
    docker_cp_into(&id, &root, "/work/bundle")?;

    world.bundle_tmp = Some(tmp);
    Ok("/work/bundle".to_string())
}

fn build_legacy_role_module(user_body: &str, has_inventory: bool) -> String {
    let mut out = String::new();
    out.push_str(
        "load(\"@bosun/builtins\", \"apt\", \"file\", \"template\", \"inventory\", \"tags\")\n\n",
    );
    if has_inventory {
        out.push_str("def main(inv):\n");
    } else {
        out.push_str("def main():\n");
    }
    // user_body может состоять из top-level вызовов; чтобы превратить его
    // в тело функции, добавляем индентацию ко всем непустым строкам.
    let body = if has_inventory {
        decorate_template_calls(user_body)
    } else {
        user_body.to_string()
    };
    for line in body.lines() {
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        out.push_str("    ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Команда `bosun apply` для сценария.
fn apply_cmd(bundle_path: &str, dry_run: bool) -> String {
    let dry = if dry_run { " --dry-run" } else { "" };
    format!(
        "bosun apply --bundle {bundle_path} --tags=bdd{dry} \
         --lock-path /tmp/bosun.lock \
         --state-dir /tmp/bosun-state \
         --log-dir /tmp/bosun-log \
         --backup-dir /tmp/bosun-backups \
         --metric-file /tmp/bosun.prom \
         --no-color",
    )
}

#[given(regex = r"^a bundle with manifest:$")]
pub async fn given_bundle_with_manifest(world: &mut BosunWorld, step: &Step) {
    let body = step
        .docstring
        .clone()
        .unwrap_or_else(|| panic!("manifest body missing in docstring"));
    world.manifest_body = Some(body);
}

#[given(regex = r"^a bundle with inventory:$")]
pub async fn given_bundle_with_inventory(world: &mut BosunWorld, step: &Step) {
    let body = step
        .docstring
        .clone()
        .unwrap_or_else(|| panic!("inventory body missing in docstring"));
    world.inventory_yaml = Some(body);
}

#[given(regex = r"^an empty inventory$")]
pub async fn given_empty_inventory(world: &mut BosunWorld) {
    world.inventory_yaml = Some("{}\n".to_string());
}

#[given(regex = r#"^the bundle has a template "([^"]+)" with content:$"#)]
pub async fn given_bundle_template(world: &mut BosunWorld, path: String, step: &Step) {
    let body = step
        .docstring
        .clone()
        .unwrap_or_else(|| panic!("template body missing in docstring"));
    world.templates.push((path, body));
}

#[when(regex = r#"^I apply the bundle$"#)]
pub async fn when_apply_bundle(world: &mut BosunWorld) {
    let bundle_path =
        materialize_and_upload_bundle(world).unwrap_or_else(|e| panic!("upload bundle: {e}"));
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"));
    let cmd = apply_cmd(&bundle_path, false);
    let res = crate::docker_helper::docker_exec_shell(&id, &cmd)
        .unwrap_or_else(|e| panic!("docker exec apply: {e}"));
    world.last_exec = Some(res);
}

#[when(regex = r#"^I apply the bundle in dry-run mode$"#)]
pub async fn when_apply_bundle_dry_run(world: &mut BosunWorld) {
    let bundle_path =
        materialize_and_upload_bundle(world).unwrap_or_else(|e| panic!("upload bundle: {e}"));
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"));
    let cmd = apply_cmd(&bundle_path, true);
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec apply: {e}"));
    world.last_exec = Some(res);
}

#[when(regex = r#"^I apply the bundle again$"#)]
pub async fn when_apply_bundle_again(world: &mut BosunWorld) {
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"));
    let cmd = apply_cmd("/work/bundle", false);
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec apply: {e}"));
    world.last_exec = Some(res);
}

/// Шаг для нового bundle_structure.feature: на месте материализует bundle
/// из таблицы (path, body). body может содержать литералы `\n` для переноса
/// строк (Gherkin не поддерживает многострочные ячейки красиво).
#[given(regex = r#"^a bundle structure under "([^"]+)":$"#)]
pub async fn given_bundle_structure(
    world: &mut BosunWorld,
    _path_in_container: String,
    step: &Step,
) {
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"));
    let table = step
        .table
        .as_ref()
        .unwrap_or_else(|| panic!("bundle structure step requires a table"));

    let tmp = TempDir::new().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let root = tmp.path().join("bundle");
    fs::create_dir_all(&root).unwrap_or_else(|e| panic!("mkdir: {e}"));

    for row in table.rows.iter().skip(1) {
        if row.len() != 2 {
            panic!("expected 2 columns, got {}: {row:?}", row.len());
        }
        let rel = &row[0];
        let raw = row[1].replace("\\n", "\n");
        let body = if rel == "bundle.toml" {
            bundle_toml_from_json_blob(&raw)
        } else {
            raw
        };
        write_file(&root.join(rel), &body).unwrap_or_else(|e| panic!("write {rel}: {e}"));
    }

    let res = docker_exec_shell(&id, "rm -rf /work/bundle && mkdir -p /work")
        .unwrap_or_else(|e| panic!("clear bundle: {e}"));
    if res.exit_code != 0 {
        panic!("clear /work/bundle: {}", res.stderr);
    }
    docker_cp_into(&id, &root, "/work/bundle").unwrap_or_else(|e| panic!("docker cp: {e}"));
    world.bundle_tmp = Some(tmp);
}

/// Превращает JSON-blob в правильный bundle.toml. Поддерживаемые ключи:
/// name, version, requires_bosun, entry, tags, inv_strategy.
fn bundle_toml_from_json_blob(json_str: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(json_str.trim())
        .unwrap_or_else(|e| panic!("invalid bundle.toml json '{json_str}': {e}"));
    let obj = v
        .as_object()
        .unwrap_or_else(|| panic!("bundle.toml json must be object"));
    let mut out = String::new();
    out.push_str("[bundle]\n");
    for key in ["name", "version", "description", "requires_bosun", "entry"] {
        if let Some(val) = obj.get(key) {
            if let Some(s) = val.as_str() {
                out.push_str(&format!("{key} = \"{s}\"\n"));
            }
        }
    }
    if let Some(strategy) = obj.get("inv_strategy").and_then(|v| v.as_str()) {
        out.push_str("\n[bundle.inventory]\n");
        out.push_str(&format!("default_merge_strategy = \"{strategy}\"\n"));
    }
    if let Some(tags) = obj.get("tags").and_then(|v| v.as_object()) {
        out.push_str("\n[bundle.tags]\n");
        for (k, v) in tags {
            let desc = v.as_str().unwrap_or("");
            out.push_str(&format!("{k} = \"{desc}\"\n"));
        }
    }
    out
}

/// Применить bundle с указанными тэгами. `tags_csv` — строка, может быть пустой.
#[when(regex = r#"^I apply the bundle with tags "([^"]*)"$"#)]
pub async fn when_apply_bundle_with_tags(world: &mut BosunWorld, tags_csv: String) {
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"));
    let tags_arg = if tags_csv.is_empty() {
        String::new()
    } else {
        format!(" --tags={tags_csv}")
    };
    let cmd = format!(
        "bosun apply --bundle /work/bundle{tags_arg} \
         --lock-path /tmp/bosun.lock \
         --state-dir /tmp/bosun-state \
         --log-dir /tmp/bosun-log \
         --backup-dir /tmp/bosun-backups \
         --metric-file /tmp/bosun.prom \
         --no-color",
    );
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec apply: {e}"));
    world.last_exec = Some(res);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn decorate_template_adds_inv_kwarg_when_missing() {
        let s = decorate_template_calls("x = template(\"foo.j2\")");
        assert!(s.contains("template(\"foo.j2\", inv = inv)"));
    }

    #[test]
    fn decorate_template_leaves_kwargs_alone() {
        let s = decorate_template_calls("x = template(\"foo.j2\", inv = something)");
        assert_eq!(s, "x = template(\"foo.j2\", inv = something)");
    }

    #[test]
    fn assemble_inserts_load_preamble_when_missing() {
        let s = assemble_manifest("apt.package(name = \"x\")\n", false);
        assert!(s.starts_with("load(\"@bosun/builtins\""));
    }
}
