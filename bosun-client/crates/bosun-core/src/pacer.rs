//! Pacer — клиент-сайд throttle между apply ресурсов.
//!
//! Зачем: на парке в 60k нод одновременный bosun apply создаёт всплеск
//! нагрузки на apt-зеркала, runr и systemd daemons. Старый chiit размазывал
//! 4-секундный run до ~30 секунд через `pacer.Tick()` с интервалом
//! 60-100 мс между шагами; bosun без этого механизма сам становится
//! источником burst-нагрузки.
//!
//! Контракт: orchestrator после каждого ресурса (кроме последнего) ждёт
//! `interval_for(n)`, где `n` — общее число ресурсов в прогоне. Интервал
//! считается как `target / n`, ограниченный сверху и снизу
//! `min_interval` / `max_interval`. Sleep уважает cancel-токен и deadline.
//!
//! Pacer выключен по умолчанию (`target = 0`): поведение `bosun apply` без
//! флагов остаётся идентичным предыдущей фазе. Включают через CLI флаг
//! `--pacer-target-sec`.

use std::time::Duration;

/// Конфигурация pacer'а. Передаётся в `Orchestrator::apply` через
/// `ApplyOpts.pacer`. Сама по себе sleep'ы не делает — только хранит
/// параметры и считает интервал по числу ресурсов.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct PacerConfig {
    /// Целевая длительность размазывания прогона. `Duration::ZERO`
    /// означает «pacer выключен» — orchestrator не делает sleep между
    /// ресурсами.
    pub target: Duration,
    /// Нижняя граница интервала между ресурсами. Если расчётный интервал
    /// меньше — поднимаем до min. Защищает от вырожденного случая
    /// «много ресурсов, sleep по микросекунде» — overhead syscall'ов
    /// съест эффект.
    pub min_interval: Duration,
    /// Верхняя граница интервала между ресурсами. Если расчётный
    /// интервал больше — опускаем до max. Защищает от вырожденного
    /// случая «target большой, ресурсов мало» — без верхней границы
    /// между двумя ресурсами могло бы быть несколько минут sleep'а.
    pub max_interval: Duration,
}

impl Default for PacerConfig {
    /// По умолчанию pacer выключен.
    fn default() -> Self {
        Self::disabled()
    }
}

impl PacerConfig {
    /// Pacer выключен — orchestrator не делает sleep между ресурсами.
    /// Это backward-compat дефолт: `bosun apply` без новых флагов работает
    /// так же, как до Phase S.
    pub const fn disabled() -> Self {
        Self {
            target: Duration::ZERO,
            min_interval: Duration::ZERO,
            max_interval: Duration::ZERO,
        }
    }

    /// Конструктор из CLI-флагов (u32 в секундах и миллисекундах).
    /// Невалидные комбинации (`min > max`) разрешаем — clamp в
    /// `interval_for` сам всё выровняет: сначала `clamp(min, max)`,
    /// нижняя граница побеждает.
    pub fn new(target_sec: u32, min_interval_ms: u32, max_interval_ms: u32) -> Self {
        Self {
            target: Duration::from_secs(u64::from(target_sec)),
            min_interval: Duration::from_millis(u64::from(min_interval_ms)),
            max_interval: Duration::from_millis(u64::from(max_interval_ms)),
        }
    }

    /// `true`, если pacer должен делать sleep. Pacer выключен, когда
    /// `target == 0` — это сигнал «не размазывай вообще».
    pub fn is_enabled(&self) -> bool {
        !self.target.is_zero()
    }

    /// Интервал между ресурсами при общем числе `resource_count`.
    /// Возвращает `Duration::ZERO` если pacer выключен или ресурсов нет.
    ///
    /// Формула: `target / n`, clamp'нутая к `[min_interval, max_interval]`.
    /// `n` — это `resource_count`, потому что пауза ставится после каждого
    /// (кроме последнего); сумма пауз ≈ `(n-1) * interval ≈ target` при
    /// больших `n`. Это упрощённая модель chiit'а — точная подстройка
    /// под `(n-1)` усложняет код без видимой пользы при ≥ 10 ресурсов.
    ///
    /// Edge case `min > max`: clamp с такими границами тривиально
    /// возвращает `min` (Rust `Duration::clamp` падает на отладке, поэтому
    /// нормализуем явно).
    pub fn interval_for(&self, resource_count: usize) -> Duration {
        if !self.is_enabled() || resource_count == 0 {
            return Duration::ZERO;
        }
        // u32-cast безопасен: usize ≥ 1, max в практике ≤ N тысяч.
        let n = u32::try_from(resource_count).unwrap_or(u32::MAX);
        let raw = self.target / n;
        let (lo, hi) = if self.min_interval > self.max_interval {
            // Грязный CLI: min > max. Считаем, что min побеждает.
            (self.min_interval, self.min_interval)
        } else {
            (self.min_interval, self.max_interval)
        };
        raw.clamp(lo, hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_disabled() {
        let cfg = PacerConfig::default();
        assert!(!cfg.is_enabled());
        assert_eq!(cfg.interval_for(10), Duration::ZERO);
    }

    #[test]
    fn disabled_const_matches_default() {
        let a = PacerConfig::default();
        let b = PacerConfig::disabled();
        assert_eq!(a.target, b.target);
        assert_eq!(a.min_interval, b.min_interval);
        assert_eq!(a.max_interval, b.max_interval);
    }

    #[test]
    fn new_from_cli_args_builds_durations() {
        let cfg = PacerConfig::new(30, 60, 100);
        assert_eq!(cfg.target, Duration::from_secs(30));
        assert_eq!(cfg.min_interval, Duration::from_millis(60));
        assert_eq!(cfg.max_interval, Duration::from_millis(100));
        assert!(cfg.is_enabled());
    }

    #[test]
    fn interval_for_disabled_returns_zero() {
        let cfg = PacerConfig::new(0, 60, 100);
        assert_eq!(cfg.interval_for(10), Duration::ZERO);
    }

    #[test]
    fn interval_for_zero_resources_returns_zero() {
        let cfg = PacerConfig::new(30, 60, 100);
        assert_eq!(cfg.interval_for(0), Duration::ZERO);
    }

    #[test]
    fn interval_for_under_min_returns_min() {
        // target=1s, N=100 → 10ms < min=60ms → clamp до 60ms.
        let cfg = PacerConfig::new(1, 60, 100);
        assert_eq!(cfg.interval_for(100), Duration::from_millis(60));
    }

    #[test]
    fn interval_for_over_max_returns_max() {
        // target=300s, N=2 → 150s > max=100ms → clamp до 100ms.
        let cfg = PacerConfig::new(300, 60, 100);
        assert_eq!(cfg.interval_for(2), Duration::from_millis(100));
    }

    #[test]
    fn interval_for_in_range_returns_raw() {
        // target=30s=30000ms, N=300 → 100ms (граничит с max).
        let cfg = PacerConfig::new(30, 60, 100);
        assert_eq!(cfg.interval_for(300), Duration::from_millis(100));

        // target=30s, N=500 → 60ms (граничит с min).
        let cfg = PacerConfig::new(30, 60, 100);
        assert_eq!(cfg.interval_for(500), Duration::from_millis(60));

        // target=30s, N=375 → 80ms — посередине, без clamp.
        let cfg = PacerConfig::new(30, 60, 100);
        assert_eq!(cfg.interval_for(375), Duration::from_millis(80));
    }

    #[test]
    fn interval_for_min_above_max_falls_back_to_min() {
        // Грязный CLI: min=200ms, max=100ms. Clamp в Rust требует
        // min <= max; наш код нормализует обе границы к min.
        let cfg = PacerConfig::new(30, 200, 100);
        // target=30s, N=300 → 100ms; clamp к [200, 200] = 200ms.
        assert_eq!(cfg.interval_for(300), Duration::from_millis(200));
    }

    #[test]
    fn interval_for_single_resource_clamps_to_max() {
        // target=30s, N=1 → 30s > max=100ms → clamp до 100ms. Sleep на
        // одном ресурсе всё равно не сработает (после последнего pause
        // не нужен), но функция чистая и должна возвращать stable значение.
        let cfg = PacerConfig::new(30, 60, 100);
        assert_eq!(cfg.interval_for(1), Duration::from_millis(100));
    }
}
