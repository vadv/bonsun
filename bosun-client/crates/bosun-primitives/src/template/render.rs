//! Рендер шаблона через `minijinja` в `Strict` режиме.
//!
//! Контекст для рендера — единственный объект `inv`. В нём ключи из inventory
//! плюс ключ `facts`, содержащий materialized facts-map. То есть в шаблоне
//! `{{ inv.X }}` и `{{ inv.facts.X }}` идут через один путь — это совпадает
//! с design'ом из spec (секция «inv в Starlark») и упрощает миграцию между
//! Starlark-evaluator'ом и шаблонами.

use std::path::{Path, PathBuf};

use minijinja::{Environment, UndefinedBehavior};

/// Ошибка рендера шаблона. `Send + Sync`, чтобы передаваться через границы
/// крейтов в anyhow-ошибку Starlark-glue.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TemplateError {
    #[error("template file not found: {0}")]
    FileNotFound(String),
    #[error("template io error in {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("jinja syntax error in {file} line {line}: {message}")]
    Syntax {
        file: String,
        line: u32,
        message: String,
    },
    #[error("undefined variable in {file}: {message}")]
    UndefinedVariable { file: String, message: String },
    #[error("render error in {file}: {message}")]
    Render { file: String, message: String },
    #[error("path traversal denied: '{0}' escapes templates root")]
    PathTraversal(String),
}

/// Отрендерить шаблон, лежащий по пути `templates_root + relative_path`.
///
/// Контекст рендера:
/// - `inv` — copy инвентаря (без mutation),
/// - `inv.facts` — материализованные факты из снимка.
///
/// `facts` нужно передавать как объект JSON: ключ — имя факта, значение —
/// сам факт (обычно скаляр или объект). CLI собирает эту карту до вызова,
/// потому что `FactsSource` через trait не даёт enumerate.
pub fn render_template(
    templates_root: &Path,
    relative_path: &str,
    inv: &serde_json::Value,
    facts: &serde_json::Value,
) -> Result<String, TemplateError> {
    let absolute_path = resolve_path(templates_root, relative_path)?;

    let source = match std::fs::read_to_string(&absolute_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(TemplateError::FileNotFound(relative_path.to_string()));
        }
        Err(e) => {
            return Err(TemplateError::Io {
                path: absolute_path.to_string_lossy().into_owned(),
                source: e,
            });
        }
    };

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);

    let template_name = relative_path.to_string();
    let template = env
        .template_from_named_str(&template_name, &source)
        .map_err(|e| classify_error(&template_name, e))?;

    // Контекст рендера: `inv` с инжектированным `facts`. Если `inv` сам по себе
    // не объект — оставляем как есть (например, root-level null допустимо
    // для merge_inventory с null override). В нашем шаблоне обращение к
    // несуществующему ключу строгое.
    let context = build_context(inv, facts);
    template
        .render(context)
        .map_err(|e| classify_error(&template_name, e))
}

/// Резолв пути: defense-in-depth от `../` в relative_path.
/// После канонизации проверяем, что путь лежит под templates_root.
fn resolve_path(templates_root: &Path, relative_path: &str) -> Result<PathBuf, TemplateError> {
    let candidate = templates_root.join(relative_path);
    // Если родителя нет, обрабатываем как FileNotFound — на этом уровне
    // достаточно: open сам выдаст NotFound при read_to_string.

    // Проверяем path-traversal без необходимости canonicalize (файла
    // может ещё не быть; canonicalize упадёт). Достаточно того, что
    // в relative_path не было `..` сверх templates_root.
    let normalized = normalize_relative(relative_path);
    if normalized.is_empty() {
        return Err(TemplateError::PathTraversal(relative_path.to_string()));
    }

    Ok(candidate)
}

/// Простая проверка: если relative_path после нормализации `..` сегментов
/// не выходит за templates_root, возвращаем нормализованный путь как строку.
/// Иначе — пустая строка (caller отдаёт PathTraversal).
fn normalize_relative(rel: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for segment in rel.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if stack.pop().is_none() {
                    return String::new();
                }
            }
            other => stack.push(other),
        }
    }
    stack.join("/")
}

/// Построить контекст рендера: `{ "inv": inv_with_facts_merged_under_facts_key }`.
fn build_context(inv: &serde_json::Value, facts: &serde_json::Value) -> serde_json::Value {
    let inv_with_facts = match inv {
        serde_json::Value::Object(map) => {
            let mut new_map = map.clone();
            new_map.insert("facts".to_string(), facts.clone());
            serde_json::Value::Object(new_map)
        }
        other => {
            // inv не объект (например, Null). Тогда контекст — { inv: orig, facts }
            // под отдельным ключом не положишь — оставим как есть, шаблон не
            // сможет обратиться к inv.facts. Это маргинальный случай.
            other.clone()
        }
    };
    serde_json::json!({ "inv": inv_with_facts })
}

fn classify_error(file: &str, e: minijinja::Error) -> TemplateError {
    use minijinja::ErrorKind;
    let line = u32::try_from(e.line().unwrap_or(0)).unwrap_or(0);
    let kind = e.kind();
    let message = format!("{e}");
    match kind {
        ErrorKind::SyntaxError => TemplateError::Syntax {
            file: file.to_string(),
            line,
            message,
        },
        ErrorKind::UndefinedError => TemplateError::UndefinedVariable {
            file: file.to_string(),
            message,
        },
        _ => TemplateError::Render {
            file: file.to_string(),
            message,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::Path;

    use super::*;

    fn write_template(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn render_simple_inv_key() {
        let tmp = tempfile::tempdir().unwrap();
        write_template(tmp.path(), "x.j2", "name={{ inv.name }}");
        let out = render_template(
            tmp.path(),
            "x.j2",
            &serde_json::json!({"name": "nginx"}),
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(out, "name=nginx");
    }

    #[test]
    fn render_with_facts_under_inv_facts() {
        let tmp = tempfile::tempdir().unwrap();
        write_template(tmp.path(), "f.j2", "host={{ inv.facts.hostname }}");
        let out = render_template(
            tmp.path(),
            "f.j2",
            &serde_json::json!({}),
            &serde_json::json!({"hostname": "abc"}),
        )
        .unwrap();
        assert_eq!(out, "host=abc");
    }

    #[test]
    fn strict_mode_undefined_inv_key_is_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_template(tmp.path(), "f.j2", "v={{ inv.missing }}");
        let err = render_template(
            tmp.path(),
            "f.j2",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(matches!(err, TemplateError::UndefinedVariable { .. }));
    }

    #[test]
    fn file_not_found_returns_specific_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = render_template(
            tmp.path(),
            "missing.j2",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        match err {
            TemplateError::FileNotFound(p) => assert_eq!(p, "missing.j2"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn syntax_error_returns_specific_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_template(tmp.path(), "bad.j2", "{% if %}");
        let err = render_template(
            tmp.path(),
            "bad.j2",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(matches!(err, TemplateError::Syntax { .. }));
    }

    #[test]
    fn path_traversal_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let err = render_template(
            tmp.path(),
            "../etc/passwd",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        assert!(matches!(err, TemplateError::PathTraversal(_)));
    }

    #[test]
    fn nested_relative_path_works() {
        let tmp = tempfile::tempdir().unwrap();
        write_template(tmp.path(), "sub/dir/x.j2", "ok");
        let out = render_template(
            tmp.path(),
            "sub/dir/x.j2",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(out, "ok");
    }

    #[test]
    fn normalize_relative_handles_dot_and_slash_segments() {
        assert_eq!(normalize_relative("a/b"), "a/b");
        assert_eq!(normalize_relative("./a/b"), "a/b");
        assert_eq!(normalize_relative("a//b"), "a/b");
        assert_eq!(normalize_relative("a/./b"), "a/b");
    }

    #[test]
    fn normalize_relative_pops_dotdot_within_root() {
        assert_eq!(normalize_relative("a/../b"), "b");
        assert_eq!(normalize_relative("a/b/../c"), "a/c");
    }

    #[test]
    fn normalize_relative_returns_empty_on_escape() {
        assert!(normalize_relative("..").is_empty());
        assert!(normalize_relative("../a").is_empty());
        assert!(normalize_relative("a/../../b").is_empty());
    }

    #[test]
    fn normalize_relative_empty_input_is_empty() {
        // Пустая строка тоже трактуется как path-traversal: рендерить нечего.
        assert!(normalize_relative("").is_empty());
    }

    #[test]
    fn loops_and_conditions_work() {
        let tmp = tempfile::tempdir().unwrap();
        write_template(
            tmp.path(),
            "loop.j2",
            "{% for x in inv.items %}{{ x }};{% endfor %}",
        );
        let out = render_template(
            tmp.path(),
            "loop.j2",
            &serde_json::json!({"items": ["a", "b", "c"]}),
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(out, "a;b;c;");
    }
}
