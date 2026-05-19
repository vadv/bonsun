//! Подготовка bundle'а на хосте и заливка в контейнер.
//!
//! Сценарии bundle_structure.feature ссылаются на готовые фикстуры из
//! `tests/bdd/data/bundles/<slug>/`. Helper копирует директорию в контейнер
//! через `docker cp`. Никаких inline-докстрингов и парсинга таблиц — bundle
//! живёт как реальные файлы на диске, его можно открыть, запустить вне docker,
//! проверить в IDE.
//!
//! Для legacy-сценариев (apt_package, file_content, template, idempotency и
//! т.д.), которые описывают manifest/inventory/template через docstring-блоки,
//! работает прежняя ветка `materialize_and_upload_bundle`.

use std::fs;
use std::path::{Path, PathBuf};

use cucumber::{gherkin::Step, given, when};
use tempfile::TempDir;

use crate::docker_helper::{docker_cp_into, docker_exec_shell};
use crate::world::BosunWorld;

const DEFAULT_BUNDLE_TOML: &str = r#"[bundle]
name = "bdd-bundle"
version = "0.1.0"
requires_bosun = ">=0.1, <1.0"

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
/// Используется только для legacy-сценариев (apt_package.feature и т.п.),
/// которые описывают manifest как docstring.
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
            let mut depth = 1;
            let mut j = i + 9;
            let content_start = j;
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
    write_file(&root.join("main.star"), &manifest)?;

    if let Some(inv_body) = &world.inventory_yaml {
        write_file(&root.join("inventory/legacy.yaml"), inv_body)?;
    }

    // Если есть шаблоны, оборачиваем user-body в роль `legacy` и кладём
    // templates рядом — module-relative template() их находит, top-level
    // template() в манифесте запрещён.
    if !world.templates.is_empty() {
        let role_body = build_legacy_role_module(&user_body, has_inventory);
        write_file(&root.join("roles/legacy/main.star"), &role_body)?;
        for (rel, body) in &world.templates {
            write_file(&root.join("roles/legacy/templates").join(rel), body)?;
        }
        let main = if has_inventory {
            "load(\"@bosun/builtins\", \"inventory\")\nload(\"@roles/legacy\", \"main\")\ninv = inventory.read(\"inventory/legacy.yaml\")\nmain(inv = inv)\n"
        } else {
            "load(\"@roles/legacy\", \"main\")\nmain()\n"
        };
        write_file(&root.join("main.star"), main)?;
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

/// Команда `bosun apply` для сценария. `init_system_override` приходит из
/// `BosunWorld` — non-None означает, что сценарий подменил факт через шаг
/// `Given init_system override "<value>"` (например, под runr-сценарии).
fn apply_cmd(bundle_path: &str, dry_run: bool, init_system_override: Option<&str>) -> String {
    let dry = if dry_run { " --dry-run" } else { "" };
    let init_override = match init_system_override {
        Some(v) => format!(" --init-system {v}"),
        None => String::new(),
    };
    format!(
        "bosun apply --bundle {bundle_path} --tags=bdd{dry}{init_override} \
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
    let cmd = apply_cmd(&bundle_path, false, world.init_system_override.as_deref());
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
    let cmd = apply_cmd(&bundle_path, true, world.init_system_override.as_deref());
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec apply: {e}"));
    world.last_exec = Some(res);
}

#[when(regex = r#"^I apply the bundle again$"#)]
pub async fn when_apply_bundle_again(world: &mut BosunWorld) {
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"));
    let cmd = apply_cmd("/work/bundle", false, world.init_system_override.as_deref());
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec apply: {e}"));
    world.last_exec = Some(res);
}

/// Залить готовый bundle-фикстур из `tests/bdd/data/bundles/<slug>/` в
/// `/work/bundle` внутри контейнера. Slug передаётся как относительный путь
/// от `tests/bdd/data/bundles/`.
#[given(regex = r#"^the bundle "([^"]+)"$"#)]
pub async fn given_bundle_fixture(world: &mut BosunWorld, slug: String) {
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"));

    let source = fixture_dir(&slug).unwrap_or_else(|e| panic!("locate fixture '{slug}': {e}"));

    let res = docker_exec_shell(&id, "rm -rf /work/bundle && mkdir -p /work")
        .unwrap_or_else(|e| panic!("clear bundle: {e}"));
    if res.exit_code != 0 {
        panic!("clear /work/bundle: {}", res.stderr);
    }
    docker_cp_into(&id, &source, "/work/bundle").unwrap_or_else(|e| panic!("docker cp: {e}"));
    // Фиксируем bundle_tmp = None — bundle лежит как фикстура, временной
    // директории нет.
    world.bundle_tmp = None;
}

/// Залить ровно тот же путь, но из произвольного места репо (например,
/// examples/multi-role-pg/bundle/). Принимает путь относительно корня
/// проекта (workspace root).
#[given(regex = r#"^the bundle from "([^"]+)"$"#)]
pub async fn given_bundle_from_workspace(world: &mut BosunWorld, rel_path: String) {
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"));

    let source = workspace_relative(&rel_path);
    if !source.exists() {
        panic!("bundle source path does not exist: {}", source.display());
    }

    let res = docker_exec_shell(&id, "rm -rf /work/bundle && mkdir -p /work")
        .unwrap_or_else(|e| panic!("clear bundle: {e}"));
    if res.exit_code != 0 {
        panic!("clear /work/bundle: {}", res.stderr);
    }
    docker_cp_into(&id, &source, "/work/bundle").unwrap_or_else(|e| panic!("docker cp: {e}"));
    world.bundle_tmp = None;
}

fn fixture_dir(slug: &str) -> anyhow::Result<PathBuf> {
    // slug может приходить как "data/bundles/<name>" или просто "<name>";
    // нормализуем к каноническому имени поддиректории.
    let trimmed = slug.trim_start_matches("./");
    let name = trimmed.strip_prefix("data/bundles/").unwrap_or(trimmed);
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let base = Path::new(manifest_dir)
        .join("tests")
        .join("bdd")
        .join("data")
        .join("bundles")
        .join(name);
    if !base.exists() {
        anyhow::bail!("fixture directory not found: {}", base.display());
    }
    Ok(base)
}

fn workspace_relative(rel: &str) -> PathBuf {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    // CARGO_MANIFEST_DIR = .../crates/bosun-cli; workspace = .../..
    Path::new(manifest_dir).join("..").join("..").join(rel)
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
    let init_override = match world.init_system_override.as_deref() {
        Some(v) => format!(" --init-system {v}"),
        None => String::new(),
    };
    let cmd = format!(
        "bosun apply --bundle /work/bundle{tags_arg}{init_override} \
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
    #[test]
    fn decorate_template_adds_inv_kwarg_when_missing() {
        let s = super::decorate_template_calls("x = template(\"foo.j2\")");
        assert!(s.contains("template(\"foo.j2\", inv = inv)"));
    }

    #[test]
    fn decorate_template_leaves_kwargs_alone() {
        let s = super::decorate_template_calls("x = template(\"foo.j2\", inv = something)");
        assert_eq!(s, "x = template(\"foo.j2\", inv = something)");
    }

    #[test]
    fn assemble_inserts_load_preamble_when_missing() {
        let s = super::assemble_manifest("apt.package(name = \"x\")\n", false);
        assert!(s.starts_with("load(\"@bosun/builtins\""));
    }

    #[test]
    fn fixture_dir_accepts_short_slug() {
        let p = super::fixture_dir("multi-role-basic").unwrap();
        assert!(p.ends_with("data/bundles/multi-role-basic"));
        assert!(p.exists());
    }

    #[test]
    fn fixture_dir_accepts_prefixed_slug() {
        let p = super::fixture_dir("data/bundles/multi-role-basic").unwrap();
        assert!(p.ends_with("data/bundles/multi-role-basic"));
    }
}
