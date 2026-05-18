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
//! `bundle.toml` с дефолтным набором полей, копируют всё в контейнер.
//!
//! Когда сценарий задаёт `Given a bundle with inventory:`, мы пишем yaml
//! и в `defaults/main.yaml` (для merge с bundle.defaults), и одновременно
//! материализуем `/work/inv.yaml`, который передаём через `--inventory`.
//! Это обходит тот факт, что в текущем bosun-core merge_inventory(Null)
//! затирает defaults: если override-yaml задан, defaults становятся видны
//! через override-канал. См. Phase 10 follow-up.

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
"#;

/// Записать файл, создавая родительские директории.
fn write_file(path: &Path, body: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, body)?;
    Ok(())
}

/// Где внутри контейнера лежит override-inventory, когда сценарий задал его.
const INV_PATH_IN_CONTAINER: &str = "/work/inv.yaml";

/// Материализовать bundle в tempdir и скопировать в контейнер по `/work/bundle`.
/// Возвращает путь к bundle внутри контейнера.
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

    let manifest = world.manifest_body.clone().unwrap_or_default();
    write_file(&root.join("manifests/main.star"), &manifest)?;

    // defaults/main.yaml — пишем всегда, даже если пуст. Это позволяет
    // bundle.merge_inventory быть детерминированным.
    let inv_body = world.inventory_yaml.clone();
    let defaults = inv_body.clone().unwrap_or_else(|| "{}\n".to_string());
    write_file(&root.join("defaults/main.yaml"), &defaults)?;

    // Если inventory задан явно — заливаем его как override через
    // --inventory. Иначе override остаётся пустым (`{}`), что для
    // merge_json означает «не трогать defaults».
    let inv_for_override = inv_body.unwrap_or_else(|| "{}\n".to_string());
    let inv_tmp = tmp.path().join("inv.yaml");
    fs::write(&inv_tmp, &inv_for_override)?;

    fs::create_dir_all(root.join("templates"))?;
    for (rel, body) in &world.templates {
        write_file(&root.join("templates").join(rel), body)?;
    }

    // Очистить /work/bundle и /work/inv.yaml перед заливкой.
    let res = docker_exec_shell(&id, "rm -rf /work/bundle /work/inv.yaml && mkdir -p /work")?;
    if res.exit_code != 0 {
        anyhow::bail!("failed to clear /work/bundle: {}", res.stderr);
    }

    docker_cp_into(&id, &root, "/work/bundle")?;
    docker_cp_into(&id, &inv_tmp, INV_PATH_IN_CONTAINER)?;

    world.bundle_tmp = Some(tmp);
    Ok("/work/bundle".to_string())
}

/// Командная строка `bosun apply` для сценария. Возвращает строку.
fn apply_cmd(bundle_path: &str, dry_run: bool) -> String {
    let dry = if dry_run { " --dry-run" } else { "" };
    format!(
        "bosun apply --bundle {bundle_path} --inventory {INV_PATH_IN_CONTAINER}{dry} \
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
    // Перезаливать bundle не нужно — он уже в /work/bundle и /work/inv.yaml.
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"));
    let cmd = apply_cmd("/work/bundle", false);
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec apply: {e}"));
    world.last_exec = Some(res);
}
