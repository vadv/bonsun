//! Запись `bosun_active_tags{tag="<name>"} 1` в Prometheus textfile.
//!
//! При следующем запуске CLI перезапишет файл целиком, поэтому неактивные
//! тэги исчезают. Метрика выносится в отдельный файл рядом с `bosun.prom`,
//! чтобы существующий metric.rs остался неприкосновенным.

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::Path;

/// Сформировать тело метрики `bosun_active_tags{tag="<name>"} 1` для каждого
/// активного тэга. Тэги сортируются (детерминизм для diff'ов и логов).
pub fn format(tags: &BTreeSet<String>) -> String {
    let mut out = String::new();
    out.push_str("# HELP bosun_active_tags 1 per active tag passed on the CLI\n");
    out.push_str("# TYPE bosun_active_tags gauge\n");
    if tags.is_empty() {
        // Пустой набор: пишем заглушечную нулевую серию с label="", чтобы
        // node_exporter всё равно увидел метрику и не считал её пропавшей.
        out.push_str("bosun_active_tags{tag=\"\"} 0\n");
        return out;
    }
    for tag in tags {
        out.push_str(&format!(
            "bosun_active_tags{{tag=\"{tag}\"}} 1\n",
            tag = escape_label(tag),
        ));
    }
    out
}

/// Атомарно записать метрику в файл. `path` — целевой файл рядом с
/// основным bosun.prom (например `bosun_tags.prom`).
pub fn write_atomic(path: &Path, tags: &BTreeSet<String>) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "tags metric path {} has no parent directory",
                path.display()
            ),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let body = format(tags);
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(body.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| std::io::Error::other(e.error))?;
    Ok(())
}

fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn format_writes_one_line_per_tag_sorted() {
        let mut tags = BTreeSet::new();
        tags.insert("production".to_string());
        tags.insert("canary".to_string());
        let s = format(&tags);
        let lines: Vec<&str> = s
            .lines()
            .filter(|l| l.starts_with("bosun_active_tags"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("canary"));
        assert!(lines[1].contains("production"));
    }

    #[test]
    fn format_emits_empty_placeholder_for_empty_set() {
        let tags = BTreeSet::new();
        let s = format(&tags);
        assert!(s.contains("bosun_active_tags{tag=\"\"} 0"));
    }

    #[test]
    fn write_atomic_creates_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("tags.prom");
        let mut tags = BTreeSet::new();
        tags.insert("staging".to_string());
        write_atomic(&path, &tags).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("bosun_active_tags{tag=\"staging\"} 1"));
    }
}
