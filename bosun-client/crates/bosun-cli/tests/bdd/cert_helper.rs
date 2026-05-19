//! Шаги проверки сертификатов и приватных ключей внутри контейнера.
//!
//! Использует openssl-binary в образе. Это позволяет проверять реальный
//! PEM-сертификат, как его прочитал бы postgres/nginx, а не Rust-парсер,
//! который мы пишем здесь же. Любое расхождение между rcgen и openssl
//! всплывёт именно через эту проверку.

use cucumber::then;

use crate::docker_helper::docker_exec_shell;
use crate::world::BosunWorld;

fn container_id_or_panic(world: &BosunWorld) -> String {
    world
        .container_id
        .clone()
        .unwrap_or_else(|| panic!("no container is running"))
}

#[then(regex = r#"^certificate at "([^"]+)" has common name "([^"]+)"$"#)]
pub async fn then_cert_common_name(world: &mut BosunWorld, path: String, expected: String) {
    let id = container_id_or_panic(world);
    let cmd =
        format!("openssl x509 -in {path} -noout -subject -nameopt RFC2253,sep_multiline,utf8");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("openssl x509: {e}"));
    if res.exit_code != 0 {
        panic!("failed to read cert {path}: {}", res.stderr);
    }
    // openssl выдаёт строки вида "CN=foo", "OU=bar" — sep_multiline разделяет
    // по новой строке; ищем именно CN.
    let mut found_cn: Option<String> = None;
    for line in res.stdout.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("CN=") {
            found_cn = Some(rest.to_string());
            break;
        }
    }
    let actual = found_cn.unwrap_or_else(|| panic!("no CN in cert subject:\n{}", res.stdout));
    if actual != expected {
        panic!("cert {path} CN mismatch: expected {expected:?}, got {actual:?}");
    }
}

#[then(regex = r#"^certificate at "([^"]+)" is valid$"#)]
pub async fn then_cert_valid(world: &mut BosunWorld, path: String) {
    let id = container_id_or_panic(world);
    // -checkend 0 → ненулевой exit, если cert просрочен прямо сейчас.
    let cmd = format!("openssl x509 -in {path} -noout -checkend 0");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("openssl x509: {e}"));
    if res.exit_code != 0 {
        panic!(
            "cert {path} is not valid (openssl exit {code}): stdout={out} stderr={err}",
            code = res.exit_code,
            out = res.stdout,
            err = res.stderr,
        );
    }
}

#[then(regex = r#"^key at "([^"]+)" has mode (\d+)$"#)]
pub async fn then_key_has_mode(world: &mut BosunWorld, path: String, mode: String) {
    let id = container_id_or_panic(world);
    let cmd = format!("stat -c '%a' {path}");
    let res = docker_exec_shell(&id, &cmd).unwrap_or_else(|e| panic!("stat: {e}"));
    if res.exit_code != 0 {
        panic!("stat failed for {path}: {}", res.stderr);
    }
    let actual = res.stdout.trim();
    if actual != mode {
        panic!("mode mismatch for key {path}: expected {mode}, got {actual}");
    }
}
