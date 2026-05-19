//! Рендер шаблона через `minijinja` в `Strict` режиме.
//!
//! Контекст для рендера — единственный объект `inv`. В нём ключи из inventory
//! плюс ключ `facts`, содержащий materialized facts-map. То есть в шаблоне
//! `{{ inv.X }}` и `{{ inv.facts.X }}` идут через один путь — это совпадает
//! с design'ом из spec (секция «inv в Starlark») и упрощает миграцию между
//! Starlark-evaluator'ом и шаблонами.

use std::path::{Component, Path, PathBuf};

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
    #[error("path traversal denied: '{path}' — {reason}")]
    PathTraversal { path: String, reason: String },
}

impl TemplateError {
    fn traversal(path: &str, reason: &'static str) -> Self {
        Self::PathTraversal {
            path: path.to_string(),
            reason: reason.to_string(),
        }
    }
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
    tracing::debug!(template = %relative_path, "rendering template");
    let absolute_path = resolve_path(templates_root, relative_path)?;

    let source = match std::fs::read_to_string(&absolute_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let err = TemplateError::FileNotFound(relative_path.to_string());
            tracing::warn!(template = %relative_path, error = %err, "render failed");
            return Err(err);
        }
        Err(e) => {
            let err = TemplateError::Io {
                path: absolute_path.to_string_lossy().into_owned(),
                source: e,
            };
            tracing::warn!(template = %relative_path, error = %err, "render failed");
            return Err(err);
        }
    };

    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);

    let template_name = relative_path.to_string();
    let template = env
        .template_from_named_str(&template_name, &source)
        .map_err(|e| classify_error(&template_name, e))
        .map_err(|err| {
            tracing::warn!(template = %relative_path, error = %err, "render failed");
            err
        })?;

    // Контекст рендера: `inv` с инжектированным `facts`. Если `inv` сам по себе
    // не объект — оставляем как есть (например, root-level null допустимо
    // для merge_inventory с null override). В нашем шаблоне обращение к
    // несуществующему ключу строгое.
    let context = build_context(inv, facts);
    template
        .render(context)
        .map_err(|e| classify_error(&template_name, e))
        .map_err(|err| {
            tracing::warn!(template = %relative_path, error = %err, "render failed");
            err
        })
}

/// Резолв пути: defense-in-depth против path-traversal и symlink-escape.
///
/// Проверки (по порядку):
/// 1. relative_path не пустой, без NUL-байта.
/// 2. relative_path не абсолютный — иначе `Path::join` отбросит templates_root.
/// 3. Ни один сегмент не равен `..` — иначе можно выйти из templates_root
///    через `Path::join` нормализацию.
/// 4. После `canonicalize` обоих путей kандидат должен начинаться с
///    canonical templates_root — это ловит хитрые случаи через `.` или
///    повторные слэши.
/// 5. `symlink_metadata` candidate'а: если запись — symlink, отказываем.
///    Symlink'и в templates/ запрещены, чтобы манифест не мог через
///    `templates/x → /etc/shadow` прочитать произвольный файл.
fn resolve_path(templates_root: &Path, relative_path: &str) -> Result<PathBuf, TemplateError> {
    if relative_path.is_empty() {
        return Err(TemplateError::traversal(relative_path, "empty path"));
    }
    if relative_path.contains('\0') {
        return Err(TemplateError::traversal(relative_path, "nul byte in path"));
    }

    let rel = Path::new(relative_path);
    if rel.is_absolute() {
        return Err(TemplateError::traversal(
            relative_path,
            "absolute paths not allowed",
        ));
    }
    for component in rel.components() {
        match component {
            Component::ParentDir => {
                return Err(TemplateError::traversal(
                    relative_path,
                    "'..' segments not allowed",
                ));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(TemplateError::traversal(
                    relative_path,
                    "absolute path components not allowed",
                ));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    let candidate = templates_root.join(rel);

    // Канонизация позволит обнаружить «boundary escape» — например, если
    // templates_root сам по себе содержит symlink и кандидат через него
    // ведёт наружу. Канонизация требует существования файла; если файла
    // нет — отдаём NotFound стандартным путём через read_to_string.
    let canonical_candidate = match std::fs::canonicalize(&candidate) {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Файла нет — read_to_string выдаст FileNotFound. До этого
            // ещё проверим, что symlink-родитель кандидата не уходит за
            // templates_root. Для простоты: тут не делаем дополнительный
            // partial-canonicalize; read_to_string сам вернёт ошибку.
            return Ok(candidate);
        }
        Err(e) => {
            return Err(TemplateError::Io {
                path: candidate.to_string_lossy().into_owned(),
                source: e,
            });
        }
    };
    let canonical_root = std::fs::canonicalize(templates_root).map_err(|e| TemplateError::Io {
        path: templates_root.to_string_lossy().into_owned(),
        source: e,
    })?;
    if !canonical_candidate.starts_with(&canonical_root) {
        return Err(TemplateError::traversal(
            relative_path,
            "resolved path escapes templates root",
        ));
    }

    // symlink_metadata на сам candidate, чтобы поймать симлинк именно в
    // листовом узле. Промежуточные symlink'и уже отсечены сравнением
    // canonical-путей выше.
    let lmeta = std::fs::symlink_metadata(&candidate).map_err(|e| TemplateError::Io {
        path: candidate.to_string_lossy().into_owned(),
        source: e,
    })?;
    if lmeta.file_type().is_symlink() {
        return Err(TemplateError::traversal(
            relative_path,
            "symlink in templates dir not allowed",
        ));
    }

    Ok(canonical_candidate)
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
        assert!(matches!(err, TemplateError::PathTraversal { .. }));
    }

    #[test]
    fn template_rejects_absolute_path() {
        let tmp = tempfile::tempdir().unwrap();
        let err = render_template(
            tmp.path(),
            "/etc/shadow",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        match err {
            TemplateError::PathTraversal { path, reason } => {
                assert_eq!(path, "/etc/shadow");
                assert!(reason.contains("absolute"));
            }
            other => panic!("expected PathTraversal, got {other:?}"),
        }
    }

    #[test]
    fn template_rejects_parent_dir_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let err = render_template(
            tmp.path(),
            "../../etc/shadow",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        match err {
            TemplateError::PathTraversal { reason, .. } => {
                assert!(reason.contains("'..'"));
            }
            other => panic!("expected PathTraversal, got {other:?}"),
        }
    }

    #[test]
    fn template_rejects_nul_byte_in_path() {
        let tmp = tempfile::tempdir().unwrap();
        let err = render_template(
            tmp.path(),
            "a\0b.j2",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        match err {
            TemplateError::PathTraversal { reason, .. } => assert!(reason.contains("nul byte")),
            other => panic!("expected PathTraversal, got {other:?}"),
        }
    }

    #[test]
    fn template_rejects_symlink_at_leaf() {
        // Symlink на файл ВНЕ templates root. canonical-сравнение должно
        // отсечь это как «escapes templates root», даже без проверки
        // is_symlink, потому что resolved path не под root.
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside-secret");
        std::fs::write(&outside, "secret data").unwrap();
        let templates_root = tmp.path().join("templates");
        std::fs::create_dir_all(&templates_root).unwrap();
        let link = templates_root.join("alias.j2");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let err = render_template(
            &templates_root,
            "alias.j2",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        match err {
            TemplateError::PathTraversal { reason, .. } => {
                assert!(
                    reason.contains("symlink") || reason.contains("escapes"),
                    "got: {reason}"
                );
            }
            other => panic!("expected PathTraversal, got {other:?}"),
        }
    }

    #[test]
    fn template_rejects_symlink_pointing_inside_root() {
        // Symlink на файл ВНУТРИ templates root. canonical-проверка не
        // поймает (resolved path под root), но is_symlink на leaf'е поймает.
        // Гарантирует, что любой symlink в templates/ запрещён.
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real.j2");
        std::fs::write(&real, "real body").unwrap();
        let link = tmp.path().join("link.j2");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = render_template(
            tmp.path(),
            "link.j2",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        match err {
            TemplateError::PathTraversal { reason, .. } => assert!(reason.contains("symlink")),
            other => panic!("expected PathTraversal, got {other:?}"),
        }
    }

    #[test]
    fn template_accepts_normal_relative() {
        let tmp = tempfile::tempdir().unwrap();
        write_template(tmp.path(), "nginx.conf.j2", "ok");
        let out = render_template(
            tmp.path(),
            "nginx.conf.j2",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(out, "ok");
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
    fn template_empty_relative_path_is_traversal_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = render_template(
            tmp.path(),
            "",
            &serde_json::json!({}),
            &serde_json::json!({}),
        )
        .unwrap_err();
        match err {
            TemplateError::PathTraversal { reason, .. } => assert!(reason.contains("empty")),
            other => panic!("expected PathTraversal, got {other:?}"),
        }
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

    #[test]
    fn render_emits_rendering_template_event() {
        use bosun_core::tracing_test_util::{install_global_router, record_events};

        install_global_router();
        let tmp = tempfile::tempdir().unwrap();
        write_template(tmp.path(), "x.j2", "ok");

        let events = record_events(|| {
            let _ = render_template(
                tmp.path(),
                "x.j2",
                &serde_json::json!({}),
                &serde_json::json!({}),
            )
            .unwrap();
        });

        assert!(
            events.iter().any(|e| e.contains("rendering template")),
            "expected 'rendering template' event; got: {events:?}",
        );
    }
}
