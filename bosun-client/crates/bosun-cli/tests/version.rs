#![allow(clippy::expect_used)]

use std::process::Command;

#[test]
fn version_prints_pkg_version() {
    let bin = env!("CARGO_BIN_EXE_bosun");
    let output = Command::new(bin)
        .arg("version")
        .output()
        .expect("binary runs");

    assert!(output.status.success(), "exit code: {:?}", output.status);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout: {stdout}"
    );
}
