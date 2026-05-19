//! Шаги проверки системных пользователей и групп внутри контейнера.
//!
//! Опираются на passwd-suite (useradd, groupadd) и getent — оба
//! устанавливаются в test-base.Dockerfile. Никаких моков, никаких
//! фейковых /etc/passwd — мы наблюдаем реальное состояние NSS.

use cucumber::then;

use crate::docker_helper::docker_exec_args;
use crate::world::BosunWorld;

fn container_id_or_panic(world: &BosunWorld) -> String {
    world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"))
}

#[then(regex = r#"^user "([^"]+)" exists in container$"#)]
pub async fn then_user_exists(world: &mut BosunWorld, name: String) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(&id, &["getent", "passwd", &name])
        .unwrap_or_else(|e| panic!("getent: {e}"));
    if res.exit_code != 0 {
        panic!(
            "user {name} not found in NSS (getent exit {code}, stderr={err})",
            code = res.exit_code,
            err = res.stderr,
        );
    }
}

#[then(regex = r#"^user "([^"]+)" does not exist in container$"#)]
pub async fn then_user_not_exists(world: &mut BosunWorld, name: String) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(&id, &["getent", "passwd", &name])
        .unwrap_or_else(|e| panic!("getent: {e}"));
    if res.exit_code == 0 {
        panic!(
            "user {name} unexpectedly exists in NSS\nentry: {entry}",
            entry = res.stdout.trim(),
        );
    }
}

#[then(regex = r#"^user "([^"]+)" has uid (\d+)$"#)]
pub async fn then_user_has_uid(world: &mut BosunWorld, name: String, expected: u32) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(&id, &["getent", "passwd", &name])
        .unwrap_or_else(|e| panic!("getent: {e}"));
    if res.exit_code != 0 {
        panic!("user {name} not found");
    }
    // passwd line: name:x:uid:gid:gecos:home:shell.
    let parts: Vec<&str> = res.stdout.trim().split(':').collect();
    let actual: u32 = parts
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("malformed passwd entry: {}", res.stdout));
    if actual != expected {
        panic!("user {name} uid mismatch: expected {expected}, got {actual}");
    }
}

#[then(regex = r#"^user "([^"]+)" has shell "([^"]+)"$"#)]
pub async fn then_user_has_shell(world: &mut BosunWorld, name: String, expected: String) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(&id, &["getent", "passwd", &name])
        .unwrap_or_else(|e| panic!("getent: {e}"));
    if res.exit_code != 0 {
        panic!("user {name} not found");
    }
    let parts: Vec<&str> = res.stdout.trim().split(':').collect();
    let actual = parts.get(6).copied().unwrap_or("");
    if actual != expected {
        panic!("user {name} shell mismatch: expected {expected:?}, got {actual:?}");
    }
}

#[then(regex = r#"^group "([^"]+)" exists in container$"#)]
pub async fn then_group_exists(world: &mut BosunWorld, name: String) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(&id, &["getent", "group", &name])
        .unwrap_or_else(|e| panic!("getent: {e}"));
    if res.exit_code != 0 {
        panic!(
            "group {name} not found in NSS (exit {code}, stderr={err})",
            code = res.exit_code,
            err = res.stderr,
        );
    }
}

#[then(regex = r#"^group "([^"]+)" does not exist in container$"#)]
pub async fn then_group_not_exists(world: &mut BosunWorld, name: String) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(&id, &["getent", "group", &name])
        .unwrap_or_else(|e| panic!("getent: {e}"));
    if res.exit_code == 0 {
        panic!(
            "group {name} unexpectedly exists\nentry: {entry}",
            entry = res.stdout.trim(),
        );
    }
}

#[then(regex = r#"^group "([^"]+)" has gid (\d+)$"#)]
pub async fn then_group_has_gid(world: &mut BosunWorld, name: String, expected: u32) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(&id, &["getent", "group", &name])
        .unwrap_or_else(|e| panic!("getent: {e}"));
    if res.exit_code != 0 {
        panic!("group {name} not found");
    }
    // group line: name:x:gid:members.
    let parts: Vec<&str> = res.stdout.trim().split(':').collect();
    let actual: u32 = parts
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("malformed group entry: {}", res.stdout));
    if actual != expected {
        panic!("group {name} gid mismatch: expected {expected}, got {actual}");
    }
}
