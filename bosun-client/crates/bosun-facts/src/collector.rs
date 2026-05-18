//! Сборщик фактов с lazy dirty-refresh.
//!
//! Архитектура:
//! - `Fact` — trait одного факта. Сборка через `collect(&FactCollectCtx)`.
//! - `FactsCollector` — владелец списка фактов и `RefCell`-кэша.
//! - `FactsSnapshot` — immutable копия кэша для Starlark-evaluation.
//! - `FactsView<'a>` — read-only ссылка на `FactsCollector`, при `get`
//!   проверяет dirty-флаг и пересобирает факт лениво.
//!
//! Single-threaded модель apply делает `RefCell` корректным выбором
//! interior mutability. Trait `Fact` не требует `Send + Sync` — симметрично
//! с `FactsSource` в bosun-core.

use std::cell::RefCell;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::time::Instant;

use bosun_core::{FactCategory, FactValue, FactsSource, RefreshPolicy, ResourceKind};

/// Контекст одного вызова `Fact::collect`. Передаёт root-путь
/// файловой системы, чтобы тесты могли подменять `/` на tempdir.
#[non_exhaustive]
pub struct FactCollectCtx {
    /// Корень файловой системы — `/` в проде, tempdir в тестах.
    /// Все коллекторы строят пути относительно него:
    /// `<root_fs>/proc/sys/kernel/hostname`, `<root_fs>/sys/fs/cgroup/...`.
    pub root_fs: PathBuf,
}

impl FactCollectCtx {
    pub fn new(root_fs: PathBuf) -> Self {
        Self { root_fs }
    }
}

/// Один факт. Сборка идёт через `collect`, который должен быть
/// тотальным: любая ошибка возвращается как `FactValue::Unknown`,
/// а паника отлавливается коллектором.
///
/// `Send + Sync` не требуется — apply однопоточный, кэш на `RefCell`.
pub trait Fact {
    fn name(&self) -> &str;
    fn category(&self) -> FactCategory;
    fn refresh_policy(&self) -> RefreshPolicy;
    fn collect(&self, ctx: &FactCollectCtx) -> FactValue;
}

/// Запись в кэше: значение + метка времени для расчёта `age` Stale-фактов
/// + dirty-флаг.
#[derive(Debug)]
pub(crate) struct CachedFact {
    pub(crate) value: FactValue,
    /// Момент перехода в текущее `Known`/`Stale` состояние. При
    /// Stale-апгрейде сохраняется исходный момент Known — это даёт
    /// честное «возраст устаревания».
    pub(crate) collected_at: Instant,
    pub(crate) dirty: bool,
}

pub struct FactsCollector {
    facts: Vec<Box<dyn Fact>>,
    cache: RefCell<HashMap<String, CachedFact>>,
    root_fs: PathBuf,
}

impl FactsCollector {
    pub fn new(root_fs: PathBuf, facts: Vec<Box<dyn Fact>>) -> Self {
        Self {
            facts,
            cache: RefCell::new(HashMap::new()),
            root_fs,
        }
    }

    pub fn root_fs(&self) -> &std::path::Path {
        &self.root_fs
    }

    /// Сборка всех `AtStart` фактов. Каждая сборка обёрнута
    /// в `catch_unwind` — паника не валит весь процесс.
    pub fn collect_at_start(&self) {
        let ctx = FactCollectCtx::new(self.root_fs.clone());
        let mut cache = self.cache.borrow_mut();
        for fact in &self.facts {
            if !matches!(fact.refresh_policy(), RefreshPolicy::AtStart) {
                continue;
            }
            let value = collect_with_panic_guard(fact.as_ref(), &ctx);
            cache.insert(
                fact.name().to_string(),
                CachedFact {
                    value,
                    collected_at: Instant::now(),
                    dirty: false,
                },
            );
        }
    }

    /// Помечает все факты с политикой `AfterApply` и `applied_kind`
    /// в `triggers` как dirty. Реальная пересборка ленивая.
    pub fn mark_dirty_after_apply(&self, applied_kind: &ResourceKind) {
        let mut cache = self.cache.borrow_mut();
        for fact in &self.facts {
            let RefreshPolicy::AfterApply { triggers } = fact.refresh_policy() else {
                continue;
            };
            if !triggers.contains(applied_kind) {
                continue;
            }
            let name = fact.name();
            if let Some(entry) = cache.get_mut(name) {
                entry.dirty = true;
                tracing::debug!(fact = name, kind = %applied_kind, "marked fact dirty");
            } else {
                // Факт ещё не собирался — кладём заглушку Unknown, чтобы
                // первый же `view.get` запустил пересборку. Это покрывает
                // случай, когда `apply_after` сработал до первого `get`.
                cache.insert(
                    name.to_string(),
                    CachedFact {
                        value: FactValue::Unknown {
                            reason: "pending after-apply refresh".to_string(),
                        },
                        collected_at: Instant::now(),
                        dirty: true,
                    },
                );
                tracing::debug!(fact = name, kind = %applied_kind, "scheduled fact for first collection after apply");
            }
        }
    }

    /// Иммутабельный снапшот текущего кэша. Используется в Starlark-evaluation,
    /// где факт-сет фиксирован.
    pub fn snapshot(&self) -> FactsSnapshot {
        let cache = self.cache.borrow();
        let map = cache
            .iter()
            .map(|(name, cached)| (name.clone(), cached.value.clone()))
            .collect();
        FactsSnapshot { facts: map }
    }

    /// Mutable вью. Read-only снаружи, но внутри пересобирает dirty-факты
    /// через RefCell.
    pub fn view(&self) -> FactsView<'_> {
        FactsView { collector: self }
    }

    /// Найти Fact по имени. Линейный скан — список фиксирован при создании,
    /// и факт-имён единицы; HashMap избыточен.
    fn fact_by_name(&self, name: &str) -> Option<&dyn Fact> {
        self.facts
            .iter()
            .find(|f| f.name() == name)
            .map(|f| f.as_ref())
    }
}

/// Иммутабельный снимок фактов. Реализует `FactsSource` для evaluation.
#[derive(Clone, Debug)]
pub struct FactsSnapshot {
    facts: HashMap<String, FactValue>,
}

impl FactsSnapshot {
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.facts.keys().map(|s| s.as_str())
    }
}

impl FactsSource for FactsSnapshot {
    fn get(&self, name: &str) -> FactValue {
        match self.facts.get(name) {
            Some(v) => v.clone(),
            None => FactValue::Unknown {
                reason: format!("unknown fact '{name}'"),
            },
        }
    }
}

/// Mutable вью на FactsCollector. Lazy: при каждом `get` проверяет
/// dirty-флаг и пересобирает факт. Если пересборка возвращает Unknown,
/// предыдущее Known становится Stale.
pub struct FactsView<'a> {
    collector: &'a FactsCollector,
}

impl FactsSource for FactsView<'_> {
    fn get(&self, name: &str) -> FactValue {
        // Шаг 1: посмотреть кэш под &-borrow и решить, нужна ли пересборка.
        // Borrow отпускается перед collect, чтобы collect мог при необходимости
        // переиспользовать FactsCollector (защитная практика для будущих
        // расширений).
        {
            let cache = self.collector.cache.borrow();
            match cache.get(name) {
                None => {
                    return FactValue::Unknown {
                        reason: format!("unknown fact '{name}'"),
                    };
                }
                Some(cached) if !cached.dirty => return cached.value.clone(),
                Some(_) => {}
            }
        }

        // Шаг 2: пересобрать факт. Если по имени Fact отсутствует
        // (не должно происходить — записи кэша создаются только из списка),
        // возвращаем Unknown.
        let fact = match self.collector.fact_by_name(name) {
            Some(f) => f,
            None => {
                return FactValue::Unknown {
                    reason: format!("fact '{name}' has cache entry but no collector"),
                };
            }
        };
        let ctx = FactCollectCtx::new(self.collector.root_fs.clone());
        let fresh = collect_with_panic_guard(fact, &ctx);

        // Шаг 3: применить правило «Unknown после Known → Stale».
        let mut cache = self.collector.cache.borrow_mut();
        let Some(entry) = cache.get_mut(name) else {
            // Между шагами 1 и 3 кэш не должен опустеть — RefCell
            // single-threaded. Если всё-таки опустел, возвращаем fresh.
            return fresh;
        };

        let new_value = upgrade_or_replace(&entry.value, fresh, entry.collected_at);

        // При апгрейде Known → Stale возраст считается от старого
        // collected_at, поэтому время не двигаем. При свежем Known
        // или Unknown — обновляем метку.
        if !matches!(new_value, FactValue::Stale { .. }) {
            entry.collected_at = Instant::now();
        }
        entry.value = new_value.clone();
        entry.dirty = false;
        new_value
    }
}

impl<'a> FactsView<'a> {
    pub fn new(collector: &'a FactsCollector) -> Self {
        Self { collector }
    }
}

/// Сборка факта с защитой от паники. Любая паника превращается
/// в `FactValue::Unknown { reason: "panic: ..." }`.
fn collect_with_panic_guard(fact: &dyn Fact, ctx: &FactCollectCtx) -> FactValue {
    let name = fact.name().to_string();
    let result = catch_unwind(AssertUnwindSafe(|| fact.collect(ctx)));
    match result {
        Ok(v) => v,
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            tracing::error!(fact = name, message = %message, "fact collector panicked");
            FactValue::Unknown {
                reason: format!("panic: {message}"),
            }
        }
    }
}

/// Извлечь сообщение из payload паники. Стандартные кейсы:
/// `panic!("msg")` → `&'static str`; `panic!("{x}", x = "msg")` → `String`.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

/// Применить правило перехода: новый результат + предыдущее значение → следующее.
///
/// - new Known(v) → Known(v) (свежее победило).
/// - new Unknown:
///   - prev Known(v) → Stale { value: v, age: now - prev_collected_at }.
///   - prev Stale → Stale с обновлённым age от того же origin времени.
///   - prev Unknown → Unknown (новый reason).
fn upgrade_or_replace(prev: &FactValue, fresh: FactValue, prev_collected_at: Instant) -> FactValue {
    match (prev, fresh) {
        (_, FactValue::Known(v)) => FactValue::Known(v),
        (FactValue::Known(prev_v), FactValue::Unknown { reason: _ }) => {
            let age = prev_collected_at.elapsed();
            tracing::info!(
                "refresh returned Unknown, demoted previous Known to Stale (age_ms={})",
                age.as_millis()
            );
            FactValue::Stale {
                value: prev_v.clone(),
                age_ms: age.as_millis() as u64,
            }
        }
        (FactValue::Stale { value, .. }, FactValue::Unknown { reason: _ }) => {
            let age = prev_collected_at.elapsed();
            FactValue::Stale {
                value: value.clone(),
                age_ms: age.as_millis() as u64,
            }
        }
        (FactValue::Unknown { .. }, FactValue::Unknown { reason }) => FactValue::Unknown { reason },
        // Stale на входе fresh не предусмотрено — коллекторы возвращают
        // только Known/Unknown. На всякий случай fallthrough на свежее.
        (_, fresh) => fresh,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use super::*;

    /// Простой Fact для тестов: возвращает заданное значение,
    /// инкрементируя счётчик вызовов.
    struct StubFact {
        name: &'static str,
        policy: RefreshPolicy,
        calls: Rc<Cell<u32>>,
        response: Rc<dyn Fn(u32) -> FactValue>,
    }

    impl Fact for StubFact {
        fn name(&self) -> &str {
            self.name
        }
        fn category(&self) -> FactCategory {
            FactCategory::Static
        }
        fn refresh_policy(&self) -> RefreshPolicy {
            self.policy.clone()
        }
        fn collect(&self, _ctx: &FactCollectCtx) -> FactValue {
            let n = self.calls.get() + 1;
            self.calls.set(n);
            (self.response)(n)
        }
    }

    fn new_collector(facts: Vec<Box<dyn Fact>>) -> FactsCollector {
        FactsCollector::new(PathBuf::from("/"), facts)
    }

    #[test]
    fn collect_at_start_populates_atstart_facts() {
        let calls = Rc::new(Cell::new(0));
        let f = StubFact {
            name: "hostname",
            policy: RefreshPolicy::AtStart,
            calls: calls.clone(),
            response: Rc::new(|_| FactValue::Known(serde_json::json!("test-host"))),
        };
        let c = new_collector(vec![Box::new(f)]);
        c.collect_at_start();
        assert_eq!(calls.get(), 1);
        let view = c.view();
        let v = view.get("hostname");
        assert!(v.is_known());
        assert_eq!(v.value().unwrap(), &serde_json::json!("test-host"));
        // Второй вызов не пересобирает — dirty=false.
        let _ = view.get("hostname");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn collect_at_start_skips_afterapply_facts() {
        let calls = Rc::new(Cell::new(0));
        let f = StubFact {
            name: "installed_packages",
            policy: RefreshPolicy::AfterApply {
                triggers: vec![ResourceKind::from_static("apt.package")],
            },
            calls: calls.clone(),
            response: Rc::new(|_| FactValue::Known(serde_json::json!({}))),
        };
        let c = new_collector(vec![Box::new(f)]);
        c.collect_at_start();
        assert_eq!(calls.get(), 0, "AfterApply не собираются на старте");
    }

    #[test]
    fn unknown_fact_returns_unknown_with_reason() {
        let c = new_collector(vec![]);
        let view = c.view();
        let v = view.get("nonexistent");
        match v {
            FactValue::Unknown { reason } => assert!(reason.contains("nonexistent")),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_is_immutable_after_creation() {
        let calls = Rc::new(Cell::new(0));
        let f = StubFact {
            name: "x",
            policy: RefreshPolicy::AtStart,
            calls: calls.clone(),
            response: Rc::new(|n| FactValue::Known(serde_json::json!(n))),
        };
        let c = new_collector(vec![Box::new(f)]);
        c.collect_at_start();
        let snap = c.snapshot();
        assert_eq!(snap.get("x").value().unwrap(), &serde_json::json!(1));
        // Пересобирать через snapshot нельзя — это фиксированная вью.
        // Помечаем dirty и проверяем что snapshot не изменился.
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        // Факт policy=AtStart — mark_dirty его не задевает; snapshot стабилен.
        assert_eq!(snap.get("x").value().unwrap(), &serde_json::json!(1));
    }

    #[test]
    fn mark_dirty_after_apply_triggers_recollect() {
        let calls = Rc::new(Cell::new(0));
        let f = StubFact {
            name: "installed_packages",
            policy: RefreshPolicy::AfterApply {
                triggers: vec![ResourceKind::from_static("apt.package")],
            },
            calls: calls.clone(),
            response: Rc::new(|n| FactValue::Known(serde_json::json!({"call": n}))),
        };
        let c = new_collector(vec![Box::new(f)]);
        // Симулируем: после первого apply помечаем dirty (factов в кэше ещё нет).
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let view = c.view();
        let v = view.get("installed_packages");
        assert_eq!(calls.get(), 1, "view.get должен пересобрать dirty факт");
        assert_eq!(v.value().unwrap()["call"], serde_json::json!(1));
        // Второй get — не пересобирает.
        let _ = view.get("installed_packages");
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn unknown_refresh_after_known_demotes_to_stale() {
        // Первый сбор: Known. После mark_dirty второй collect → Unknown.
        // Ожидаем Stale с тем же значением.
        let calls = Rc::new(Cell::new(0));
        let f = StubFact {
            name: "fluctuating",
            policy: RefreshPolicy::AfterApply {
                triggers: vec![ResourceKind::from_static("apt.package")],
            },
            calls: calls.clone(),
            response: Rc::new(|n| {
                if n == 1 {
                    FactValue::Known(serde_json::json!("first"))
                } else {
                    FactValue::Unknown {
                        reason: "transient io error".into(),
                    }
                }
            }),
        };
        let c = new_collector(vec![Box::new(f)]);
        // Первый сбор — кладём Known через mark_dirty + view.get.
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let view = c.view();
        let v1 = view.get("fluctuating");
        assert!(v1.is_known());

        // Поспим миллисекунду, чтобы age был >0.
        std::thread::sleep(Duration::from_millis(2));

        // Помечаем dirty снова и пересобираем — теперь Unknown.
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let v2 = view.get("fluctuating");
        match v2 {
            FactValue::Stale { value, age_ms } => {
                assert_eq!(value, serde_json::json!("first"));
                assert!(age_ms >= 1, "age должно быть положительным");
            }
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn stale_after_repeated_unknown_keeps_value() {
        let calls = Rc::new(Cell::new(0));
        let f = StubFact {
            name: "blip",
            policy: RefreshPolicy::AfterApply {
                triggers: vec![ResourceKind::from_static("apt.package")],
            },
            calls: calls.clone(),
            response: Rc::new(|n| {
                if n == 1 {
                    FactValue::Known(serde_json::json!("ok"))
                } else {
                    FactValue::Unknown {
                        reason: "still broken".into(),
                    }
                }
            }),
        };
        let c = new_collector(vec![Box::new(f)]);
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let view = c.view();
        let _ = view.get("blip"); // Known
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let _ = view.get("blip"); // Stale
        std::thread::sleep(Duration::from_millis(2));
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let v3 = view.get("blip");
        match v3 {
            FactValue::Stale { value, .. } => assert_eq!(value, serde_json::json!("ok")),
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn unknown_then_unknown_stays_unknown_with_new_reason() {
        let f = StubFact {
            name: "always_bad",
            policy: RefreshPolicy::AfterApply {
                triggers: vec![ResourceKind::from_static("apt.package")],
            },
            calls: Rc::new(Cell::new(0)),
            response: Rc::new(|n| FactValue::Unknown {
                reason: format!("io error #{n}"),
            }),
        };
        let c = new_collector(vec![Box::new(f)]);
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let view = c.view();
        let v1 = view.get("always_bad");
        assert!(matches!(v1, FactValue::Unknown { .. }));
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let v2 = view.get("always_bad");
        match v2 {
            FactValue::Unknown { reason } => assert!(reason.contains("#2")),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn mark_dirty_does_not_touch_non_matching_kind() {
        let calls = Rc::new(Cell::new(0));
        let f = StubFact {
            name: "tied_to_apt",
            policy: RefreshPolicy::AfterApply {
                triggers: vec![ResourceKind::from_static("apt.package")],
            },
            calls: calls.clone(),
            response: Rc::new(|_| FactValue::Known(serde_json::json!("v"))),
        };
        let c = new_collector(vec![Box::new(f)]);
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let view = c.view();
        let _ = view.get("tied_to_apt");
        assert_eq!(calls.get(), 1);
        // mark_dirty по другому kind не должен пометить факт.
        c.mark_dirty_after_apply(&ResourceKind::from_static("file.content"));
        let _ = view.get("tied_to_apt");
        assert_eq!(calls.get(), 1, "не должно быть пересборки");
    }

    #[test]
    fn known_refresh_overrides_stale() {
        // Stale → Known через успешный refresh.
        let calls = Rc::new(Cell::new(0));
        let f = StubFact {
            name: "recovers",
            policy: RefreshPolicy::AfterApply {
                triggers: vec![ResourceKind::from_static("apt.package")],
            },
            calls: calls.clone(),
            response: Rc::new(|n| match n {
                1 => FactValue::Known(serde_json::json!("v1")),
                2 => FactValue::Unknown {
                    reason: "transient".into(),
                },
                _ => FactValue::Known(serde_json::json!("v2")),
            }),
        };
        let c = new_collector(vec![Box::new(f)]);
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let view = c.view();
        assert!(view.get("recovers").is_known());
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        assert!(matches!(view.get("recovers"), FactValue::Stale { .. }));
        c.mark_dirty_after_apply(&ResourceKind::from_static("apt.package"));
        let v3 = view.get("recovers");
        match v3 {
            FactValue::Known(v) => assert_eq!(v, serde_json::json!("v2")),
            other => panic!("expected Known, got {other:?}"),
        }
    }

    #[test]
    fn panic_in_collect_yields_unknown_with_reason() {
        struct PanicFact;
        impl Fact for PanicFact {
            fn name(&self) -> &str {
                "boom"
            }
            fn category(&self) -> FactCategory {
                FactCategory::Static
            }
            fn refresh_policy(&self) -> RefreshPolicy {
                RefreshPolicy::AtStart
            }
            fn collect(&self, _ctx: &FactCollectCtx) -> FactValue {
                panic!("kaboom");
            }
        }
        let c = new_collector(vec![Box::new(PanicFact)]);
        c.collect_at_start();
        let view = c.view();
        match view.get("boom") {
            FactValue::Unknown { reason } => {
                assert!(reason.contains("panic"));
                assert!(reason.contains("kaboom"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_returns_clone_of_known_values() {
        let f = StubFact {
            name: "h",
            policy: RefreshPolicy::AtStart,
            calls: Rc::new(Cell::new(0)),
            response: Rc::new(|_| FactValue::Known(serde_json::json!({"a": 1}))),
        };
        let c = new_collector(vec![Box::new(f)]);
        c.collect_at_start();
        let snap = c.snapshot();
        let v = snap.get("h");
        assert!(v.is_known());
        // FactsSnapshot::get на отсутствующий ключ → Unknown.
        let none = snap.get("nope");
        assert!(matches!(none, FactValue::Unknown { .. }));
    }
}
