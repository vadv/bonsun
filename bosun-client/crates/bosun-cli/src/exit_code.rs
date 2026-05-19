//! Exit-коды per spec, секция «bosun-cli / Exit codes».
//!
//! Семантика подобрана так, чтобы внешний runr-таймер мог по коду решать,
//! генерировать алерт или нет. Подробности — в spec.

/// Apply без ошибок (включая «всё уже стоит»); либо `--dry-run` без drift;
/// либо flock не получен (другая инстанция активна).
pub const SUCCESS: i32 = 0;

/// Apply начался, часть ресурсов применилась, потом критическая ошибка.
pub const APPLY_PARTIAL_FAILURE: i32 = 1;

/// `--dry-run` обнаружил drift — есть pending changes (для CI-gating).
pub const DRY_RUN_DRIFT: i32 = 2;

/// Ошибка до apply: invalid manifest, отсутствует ключ inv, version mismatch,
/// bundle не загрузился.
pub const EVAL_ERROR: i32 = 3;

/// CLI/окружение: некорректные аргументы, не удалось создать state/log/backup
/// директории, не удалось открыть lock-файл.
pub const CLI_ENV_ERROR: i32 = 4;

/// `bosun status` обнаружил `*.manual_clear` файлы в journal'е — оператор
/// должен разобраться с зависшими defer'ами. Семантически это «частичный
/// провал»: код пересекается с APPLY_PARTIAL_FAILURE по значению (1), но
/// именован отдельно ради ясности интентов и алертов.
pub const STATUS_MANUAL_CLEAR_PRESENT: i32 = 1;

/// Apply прерван SIGTERM/SIGINT или истечением `--deadline-sec`.
/// 130 — POSIX-стандарт (128 + SIGINT=2). Внешний оркестратор по этому
/// коду понимает: «процесс прервали извне, попытка незавершена»,
/// в отличие от 1 («часть ресурсов реально провалилась»).
pub const APPLY_INTERRUPTED: i32 = 130;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_distinct_and_within_expected_ranges() {
        let small_codes = [
            SUCCESS,
            APPLY_PARTIAL_FAILURE,
            DRY_RUN_DRIFT,
            EVAL_ERROR,
            CLI_ENV_ERROR,
        ];
        for code in small_codes {
            assert!(
                (0..=4).contains(&code),
                "code {code} out of documented small range"
            );
        }
        // POSIX-style signal exit codes — отдельный диапазон 128+N.
        assert_eq!(APPLY_INTERRUPTED, 130);

        let all_codes: Vec<i32> = small_codes
            .iter()
            .copied()
            .chain([APPLY_INTERRUPTED])
            .collect();
        let mut sorted = all_codes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), all_codes.len(), "exit codes must be distinct");
    }

    #[test]
    fn status_manual_clear_present_overlaps_with_apply_partial_failure_by_design() {
        // Документируем намеренное совпадение значений (1): обе семантики
        // означают «оператор должен посмотреть». Если кому-то будет нужно
        // разделить — поменять STATUS_MANUAL_CLEAR_PRESENT на свободный код.
        assert_eq!(STATUS_MANUAL_CLEAR_PRESENT, APPLY_PARTIAL_FAILURE);
    }
}
