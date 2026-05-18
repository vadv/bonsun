//! Шаги-проверки состояния после `bosun apply`.
//!
//! Все проверки — через `docker exec`: dpkg-query, ls, sha256sum.
//! Никаких моков, никаких подгонок состояния — мы наблюдаем то, что
//! реально оказалось в контейнере.

use cucumber::then;

use crate::docker_helper::{docker_exec_args, docker_exec_shell};
use crate::world::BosunWorld;

fn container_id_or_panic(world: &BosunWorld) -> String {
    world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"))
}

#[then(regex = r#"^package "([^"]+)" is installed$"#)]
pub async fn then_package_installed(world: &mut BosunWorld, name: String) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(
        &id,
        &["dpkg-query", "-W", "-f", "${db:Status-Status}", &name],
    )
    .unwrap_or_else(|e| panic!("dpkg-query: {e}"));
    if res.exit_code != 0 {
        panic!(
            "package {name} not installed (dpkg-query exit {code}): stdout={out} stderr={err}",
            code = res.exit_code,
            out = res.stdout,
            err = res.stderr,
        );
    }
    let status = res.stdout.trim();
    if status != "installed" {
        panic!("package {name} has unexpected dpkg status '{status}'");
    }
}

#[then(regex = r#"^package "([^"]+)" is not installed$"#)]
pub async fn then_package_not_installed(world: &mut BosunWorld, name: String) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(
        &id,
        &["dpkg-query", "-W", "-f", "${db:Status-Status}", &name],
    )
    .unwrap_or_else(|e| panic!("dpkg-query: {e}"));
    // dpkg-query exit != 0 means package is not known — это и есть наш «not installed».
    if res.exit_code == 0 && res.stdout.trim() == "installed" {
        panic!("package {name} is installed, expected not installed");
    }
}

#[then(regex = r#"^file "([^"]+)" exists in container$"#)]
pub async fn then_file_exists(world: &mut BosunWorld, path: String) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(&id, &["test", "-e", &path])
        .unwrap_or_else(|e| panic!("docker exec test -e: {e}"));
    if res.exit_code != 0 {
        panic!("file {path} does not exist in container");
    }
}

#[then(regex = r#"^file "([^"]+)" does not exist in container$"#)]
pub async fn then_file_not_exists(world: &mut BosunWorld, path: String) {
    let id = container_id_or_panic(world);
    let res = docker_exec_args(&id, &["test", "-e", &path])
        .unwrap_or_else(|e| panic!("docker exec test -e: {e}"));
    if res.exit_code == 0 {
        panic!("file {path} exists in container, expected not to exist");
    }
}

#[then(regex = r#"^file "([^"]+)" has content "([^"]*)"$"#)]
pub async fn then_file_has_content(world: &mut BosunWorld, path: String, expected: String) {
    let id = container_id_or_panic(world);
    let res =
        docker_exec_args(&id, &["cat", &path]).unwrap_or_else(|e| panic!("docker exec cat: {e}"));
    if res.exit_code != 0 {
        panic!(
            "failed to read {path} (exit {code}): {err}",
            code = res.exit_code,
            err = res.stderr,
        );
    }
    if res.stdout != expected {
        panic!(
            "file {path} content mismatch\nexpected: {expected:?}\nactual:   {actual:?}",
            actual = res.stdout,
        );
    }
}

#[then(regex = r#"^file "([^"]+)" contains "([^"]+)"$"#)]
pub async fn then_file_contains(world: &mut BosunWorld, path: String, needle: String) {
    let id = container_id_or_panic(world);
    let res =
        docker_exec_args(&id, &["cat", &path]).unwrap_or_else(|e| panic!("docker exec cat: {e}"));
    if res.exit_code != 0 {
        panic!(
            "failed to read {path} (exit {code}): {err}",
            code = res.exit_code,
            err = res.stderr,
        );
    }
    if !res.stdout.contains(&needle) {
        panic!(
            "file {path} does not contain {needle:?}\nactual content:\n{actual}",
            actual = res.stdout,
        );
    }
}

#[then(regex = r#"^file "([^"]+)" has sha256 "([0-9a-f]+)"$"#)]
pub async fn then_file_has_sha(world: &mut BosunWorld, path: String, expected_hex: String) {
    let id = container_id_or_panic(world);
    let cmd = format!("sha256sum {path}");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("sha256sum: {e}"));
    if res.exit_code != 0 {
        panic!("sha256sum failed for {path}: {}", res.stderr);
    }
    let actual = res.stdout.split_whitespace().next().unwrap_or("");
    if actual != expected_hex {
        panic!("sha256 mismatch for {path}\nexpected: {expected_hex}\nactual:   {actual}",);
    }
}

#[then(regex = r#"^file "([^"]+)" has mode (\d+)$"#)]
pub async fn then_file_has_mode(world: &mut BosunWorld, path: String, mode: String) {
    let id = container_id_or_panic(world);
    let cmd = format!("stat -c '%a' {path}");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("stat: {e}"));
    if res.exit_code != 0 {
        panic!("stat failed for {path}: {}", res.stderr);
    }
    let actual = res.stdout.trim();
    if actual != mode {
        panic!("mode mismatch for {path}: expected {mode}, got {actual}");
    }
}

#[then(regex = r#"^file "([^"]+)" has owner "([^"]+)"$"#)]
pub async fn then_file_has_owner(world: &mut BosunWorld, path: String, expected: String) {
    let id = container_id_or_panic(world);
    let cmd = format!("stat -c '%U' {path}");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("stat: {e}"));
    if res.exit_code != 0 {
        panic!("stat owner failed for {path}: {}", res.stderr);
    }
    let actual = res.stdout.trim();
    if actual != expected {
        panic!("owner mismatch for {path}: expected {expected}, got {actual}");
    }
}

#[then(regex = r#"^there are (\d+) backup files in "([^"]+)"$"#)]
pub async fn then_backup_count(world: &mut BosunWorld, count: usize, dir: String) {
    let id = container_id_or_panic(world);
    let cmd = format!("ls -1 {dir} 2>/dev/null | wc -l");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("ls/wc: {e}"));
    if res.exit_code != 0 {
        panic!("count backups failed for {dir}: {}", res.stderr);
    }
    let actual: usize = res
        .stdout
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("parse count {:?}: {e}", res.stdout));
    if actual != count {
        panic!(
            "backup count mismatch for {dir}: expected {count}, got {actual}\nlisting:\n{listing}",
            listing = docker_exec_shell(&id, &format!("ls -la {dir}"))
                .map(|r| r.stdout)
                .unwrap_or_default(),
        );
    }
}
