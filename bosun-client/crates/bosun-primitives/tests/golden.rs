//! Golden-тесты рендера шаблонов.
//!
//! Каждая поддиректория `tests/golden/<name>/` содержит:
//! - `template.j2` — шаблон,
//! - `inv.json` — inventory для рендера,
//! - `facts.json` — материализованные факты,
//! - `expected.txt` — ожидаемый вывод (для success-сценариев),
//! - `error_kind.txt` — имя варианта `TemplateError` (для failure-сценариев).
//!
//! Драйвер выбирает между success/failure по наличию `expected.txt`.
//! Регенерация эталонов: `UPDATE_GOLDEN=1 cargo test -p bosun-primitives --test golden`.

#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]

use std::path::Path;

use bosun_primitives::{render_template, TemplateError};

fn golden_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

fn run_success_case(name: &str) {
    let dir = golden_root().join(name);
    let inv: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("inv.json")).unwrap()).unwrap();
    let facts: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("facts.json")).unwrap()).unwrap();

    let actual = render_template(&dir, "template.j2", &inv, &facts).unwrap_or_else(|e| {
        panic!("expected success for '{name}', got error: {e:?}");
    });

    let expected_path = dir.join("expected.txt");
    if std::env::var("UPDATE_GOLDEN").is_ok() {
        std::fs::write(&expected_path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&expected_path).unwrap();
    assert_eq!(
        actual, expected,
        "golden '{name}' output differs from expected.txt"
    );
}

fn run_failure_case(name: &str) {
    let dir = golden_root().join(name);
    let inv: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("inv.json")).unwrap()).unwrap();
    let facts: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("facts.json")).unwrap()).unwrap();

    let err = render_template(&dir, "template.j2", &inv, &facts)
        .expect_err(&format!("expected failure for '{name}'"));

    let expected_kind = std::fs::read_to_string(dir.join("error_kind.txt"))
        .unwrap()
        .trim()
        .to_string();

    let actual_kind = error_variant(&err);
    assert_eq!(
        actual_kind, expected_kind,
        "golden '{name}' error kind mismatch (full error: {err:?})"
    );
}

fn error_variant(e: &TemplateError) -> &'static str {
    match e {
        TemplateError::FileNotFound(_) => "FileNotFound",
        TemplateError::Io { .. } => "Io",
        TemplateError::Syntax { .. } => "Syntax",
        TemplateError::UndefinedVariable { .. } => "UndefinedVariable",
        TemplateError::Render { .. } => "Render",
        TemplateError::PathTraversal(_) => "PathTraversal",
        // non_exhaustive: будущие варианты упадут в тесте, не молча совпадая
        // с произвольным variant'ом из expected_kind.
        _ => "Unknown",
    }
}

#[test]
fn golden_basic_render() {
    run_success_case("basic_render");
}

#[test]
fn golden_with_facts() {
    run_success_case("with_facts");
}

#[test]
fn golden_missing_inv_key() {
    run_failure_case("missing_inv_key");
}
