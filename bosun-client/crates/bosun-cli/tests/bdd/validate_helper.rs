//! Шаги вокруг `bosun bundle validate`.
//!
//! Validate — статическая проверка bundle без обращения к системе. Принимает
//! `--facts fixture.json`, что позволяет тестировать диспатчер `service.unit`
//! без запуска реальной init-системы: достаточно подсунуть факт
//! `init_system = "systemd"` через fixture.
//!
//! Для самих ресурсов validate не делает plan/apply, только evaluate
//! manifest'а и регистрацию через mock-примитивы. Это значит, что:
//! - `service.unit` с неподходящими kwargs упадёт на evaluate (через
//!   `reject_unexpected_service_unit_kwargs`);
//! - `service.unit` с валидными kwargs — пройдёт и зарегистрирует
//!   соответствующий ресурс (systemd.service / runr.service);
//! - `runr.service` без runr-фактов всё равно пройдёт — validate не
//!   проверяет init-доступность.

use std::fs;

use cucumber::{given, when};
use tempfile::TempDir;

use crate::bundle_helper::materialize_and_upload_bundle;
use crate::docker_helper::{docker_cp_into, docker_exec_shell};
use crate::world::BosunWorld;

fn container_id_or_panic(world: &BosunWorld) -> String {
    world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"))
}

#[given(regex = r#"^facts fixture init_system = "([^"]+)"$"#)]
pub async fn given_facts_fixture(world: &mut BosunWorld, value: String) {
    let id = container_id_or_panic(world);

    let tmp = TempDir::new().unwrap_or_else(|e| panic!("create tmp: {e}"));
    let path = tmp.path().join("facts.json");
    let body = serde_json::json!({ "init_system": value }).to_string();
    fs::write(&path, body).unwrap_or_else(|e| panic!("write facts fixture: {e}"));

    docker_cp_into(&id, &path, "/work/facts.json")
        .unwrap_or_else(|e| panic!("docker cp facts: {e}"));

    // Держим tempdir живым через bundle_tmp (он Drop'нется в After-хуке).
    // Если уже что-то лежит в bundle_tmp (bundle уже залит) — оставляем
    // тот tempdir, а facts.json просто скопирован отдельно. Если bundle ещё
    // не залит — занимаем bundle_tmp фейково, чтобы facts.json не исчез.
    if world.bundle_tmp.is_none() {
        world.bundle_tmp = Some(tmp);
    }
    // Иначе tmp умрёт при выходе из функции, но файл уже скопирован в
    // контейнер — это нормально, на хосте он больше не нужен.
}

#[when(regex = r#"^I validate the bundle$"#)]
pub async fn when_validate_bundle(world: &mut BosunWorld) {
    let bundle_path =
        materialize_and_upload_bundle(world).unwrap_or_else(|e| panic!("upload bundle: {e}"));
    let id = container_id_or_panic(world);
    let cmd =
        format!("bosun bundle validate --bundle {bundle_path} --tags=bdd --facts /work/facts.json");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec validate: {e}"));
    world.last_exec = Some(res);
}

#[when(regex = r#"^I validate the bundle without facts fixture$"#)]
pub async fn when_validate_bundle_no_facts(world: &mut BosunWorld) {
    let bundle_path =
        materialize_and_upload_bundle(world).unwrap_or_else(|e| panic!("upload bundle: {e}"));
    let id = container_id_or_panic(world);
    let cmd = format!("bosun bundle validate --bundle {bundle_path} --tags=bdd");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec validate: {e}"));
    world.last_exec = Some(res);
}
