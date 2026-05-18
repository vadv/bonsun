//! Управление docker-контейнерами и exec'ами через CLI.
//!
//! Каждый сценарий поднимает свежий long-running контейнер через
//! `docker run -d ... tail -f /dev/null`, копирует туда bosun-бинарь и
//! опционально bundle, выполняет команды через `docker exec`. После
//! сценария — `docker rm -f`.

use std::path::Path;
use std::process::{Command, Stdio};

use cucumber::{given, then, when};

use crate::world::{BosunWorld, DockerExecResult};

const DEFAULT_IMAGE_ENV: &str = "BOSUN_TEST_IMAGE";
const DEFAULT_IMAGE_FALLBACK: &str = "bosun-test-base:latest";
const CONTAINER_WORKDIR: &str = "/work";

/// Достать имя образа из ENV (или fallback).
pub fn test_image() -> String {
    std::env::var(DEFAULT_IMAGE_ENV).unwrap_or_else(|_| DEFAULT_IMAGE_FALLBACK.to_string())
}

/// Поднять long-running контейнер. Возвращает container id.
fn docker_run_detached(image: &str) -> anyhow::Result<String> {
    let output = Command::new("docker")
        .args([
            "run",
            "-d",
            "--rm",
            "-w",
            CONTAINER_WORKDIR,
            image,
            "tail",
            "-f",
            "/dev/null",
        ])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "docker run failed: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let id = String::from_utf8(output.stdout)?.trim().to_string();
    if id.is_empty() {
        anyhow::bail!("docker run returned empty container id");
    }
    Ok(id)
}

/// Остановить контейнер (best-effort).
pub fn docker_kill(container_id: &str) {
    let _ = Command::new("docker")
        .args(["rm", "-f", container_id])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Скопировать файл с хоста в контейнер.
pub fn docker_cp_into(
    container_id: &str,
    host_src: &Path,
    container_dst: &str,
) -> anyhow::Result<()> {
    let arg = format!("{container_id}:{container_dst}");
    let output = Command::new("docker")
        .args(["cp", &host_src.to_string_lossy(), &arg])
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "docker cp {} -> {} failed: {}\n{}",
            host_src.display(),
            arg,
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Выполнить команду внутри контейнера. Шелл-форма: запускаем `sh -c '<cmd>'`,
/// чтобы можно было использовать пайпы, перенаправления и подстановки в .feature.
pub fn docker_exec_shell(container_id: &str, cmd: &str) -> anyhow::Result<DockerExecResult> {
    let output = Command::new("docker")
        .args(["exec", container_id, "sh", "-c", cmd])
        .output()?;
    Ok(DockerExecResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Прямой exec без шелл-обёртки. Каждый аргумент передаётся отдельно.
pub fn docker_exec_args(container_id: &str, args: &[&str]) -> anyhow::Result<DockerExecResult> {
    let mut full: Vec<&str> = vec!["exec", container_id];
    full.extend_from_slice(args);
    let output = Command::new("docker").args(&full).output()?;
    Ok(DockerExecResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Сделать `chmod +x` для bosun внутри контейнера.
fn install_bosun_binary(container_id: &str, host_binary: &Path) -> anyhow::Result<()> {
    docker_cp_into(container_id, host_binary, "/usr/local/bin/bosun")?;
    let res = docker_exec_args(container_id, &["chmod", "+x", "/usr/local/bin/bosun"])?;
    if res.exit_code != 0 {
        anyhow::bail!("chmod +x bosun failed: {}", res.stderr);
    }
    Ok(())
}

/// Подготовить bosun-бинарь: либо взять из ENV `BOSUN_BINARY`, либо найти
/// в `target/{release,debug}/bosun` relative to CARGO_MANIFEST_DIR.
pub fn locate_bosun_binary() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(p) = std::env::var("BOSUN_BINARY") {
        let pb = std::path::PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
        anyhow::bail!("BOSUN_BINARY={} but file does not exist", pb.display());
    }
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let workspace = Path::new(manifest_dir).join("../..");
    for profile in ["release", "debug"] {
        let candidate = workspace.join("target").join(profile).join("bosun");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "bosun binary not found; set BOSUN_BINARY or run `cargo build --release -p bosun-cli`"
    )
}

#[given(regex = r#"^a fresh container$"#)]
pub async fn given_fresh_container(world: &mut BosunWorld) {
    if let Some(id) = world.container_id.take() {
        docker_kill(&id);
    }
    let image = test_image();
    let id = docker_run_detached(&image).unwrap_or_else(|e| {
        panic!("failed to start container from {image}: {e}");
    });
    let bin = world.bosun_binary_path.as_path().to_path_buf();
    let bin = if bin.as_os_str().is_empty() {
        locate_bosun_binary().unwrap_or_else(|e| panic!("locate bosun: {e}"))
    } else {
        bin
    };
    if let Err(e) = install_bosun_binary(&id, &bin) {
        docker_kill(&id);
        panic!("install bosun into container: {e}");
    }
    world.bosun_binary_path = bin;
    world.container_id = Some(id);
    world.container_workdir = CONTAINER_WORKDIR.to_string();
}

#[when(regex = r#"^I run "([^"]+)" inside the container$"#)]
pub async fn when_run_inside_container(world: &mut BosunWorld, cmd: String) {
    let id = world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running; add `Given a fresh container`"));
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("docker exec failed: {e}"));
    world.last_exec = Some(res);
}

#[then(regex = r#"^exit code is (-?\d+)$"#)]
pub async fn then_exit_code(world: &mut BosunWorld, expected: i32) {
    let res = world
        .last_exec
        .as_ref()
        .unwrap_or_else(|| panic!("no command has been run yet"));
    if res.exit_code != expected {
        panic!(
            "exit code mismatch: expected {expected}, got {actual}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            actual = res.exit_code,
            stdout = res.stdout,
            stderr = res.stderr,
        );
    }
}

#[then(regex = r#"^stdout contains "([^"]+)"$"#)]
pub async fn then_stdout_contains(world: &mut BosunWorld, needle: String) {
    let res = world
        .last_exec
        .as_ref()
        .unwrap_or_else(|| panic!("no command has been run yet"));
    if !res.stdout.contains(&needle) {
        panic!(
            "stdout does not contain expected text\nneedle: {needle}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            stdout = res.stdout,
            stderr = res.stderr,
        );
    }
}

#[then(regex = r#"^stderr contains "([^"]+)"$"#)]
pub async fn then_stderr_contains(world: &mut BosunWorld, needle: String) {
    let res = world
        .last_exec
        .as_ref()
        .unwrap_or_else(|| panic!("no command has been run yet"));
    if !res.stderr.contains(&needle) {
        panic!(
            "stderr does not contain expected text\nneedle: {needle}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            stdout = res.stdout,
            stderr = res.stderr,
        );
    }
}

#[then(regex = r#"^output contains "([^"]+)"$"#)]
pub async fn then_output_contains(world: &mut BosunWorld, needle: String) {
    let res = world
        .last_exec
        .as_ref()
        .unwrap_or_else(|| panic!("no command has been run yet"));
    let combined = res.combined();
    if !combined.contains(&needle) {
        panic!(
            "combined output does not contain expected text\nneedle: {needle}\noutput:\n{combined}",
        );
    }
}
