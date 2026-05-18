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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_distinct_in_documented_range() {
        let codes = [
            SUCCESS,
            APPLY_PARTIAL_FAILURE,
            DRY_RUN_DRIFT,
            EVAL_ERROR,
            CLI_ENV_ERROR,
        ];
        for code in codes {
            assert!(
                (0..=4).contains(&code),
                "code {code} out of documented range"
            );
        }
        // Уникальность — простая защита от опечаток в константах.
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len(), "exit codes must be distinct");
    }
}
