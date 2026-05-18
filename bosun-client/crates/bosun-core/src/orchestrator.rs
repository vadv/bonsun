//! Orchestrator — plan_only и apply поверх Registry + примитивов.
//!
//! Согласно спеке («Plan / Apply / Dry-run»):
//! - `plan_only` идёт по `topological_order`, вызывает `primitive.plan` и
//!   складывает diff в `PlanReport`. Никаких apply.
//! - `apply` идёт по тому же порядку, для каждого ресурса plan → apply.
//!   Если `diff == NoChange`, apply пропускается. Иначе СРАЗУ ДО `primitive.apply`
//!   вызывается callback `mark_dirty` — это закрывает баг «failed apply
//!   оставлял зависимые факты устаревшими».
//!
//! Cyclic-dep limitation: `bosun-core` не может зависеть от `bosun-facts`,
//! поэтому интеграция с `FactsCollector::mark_dirty_after_apply` идёт через
//! callback `&dyn Fn(&ResourceKind)`, который вызывающий бэкэнд (CLI) связывает
//! с реальной коллекцией.

use std::collections::HashMap;

use serde::Serialize;

use crate::diff::Diff;
use crate::primitive::{ApplyCtx, FactsSource, PlanCtx, Primitive, PrimitiveError};
use crate::registry::{Registry, RegistryError};
use crate::resource::{ResourceId, ResourceKind};

/// Опции apply.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ApplyOpts {
    /// Если true — после Err продолжаем выполнение остальных ресурсов,
    /// собираем ошибки в отчёт. Если false — первый Err прерывает прогон.
    pub continue_on_error: bool,
}

/// План одного ресурса.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ResourcePlan {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub diff: Diff,
    /// Аннотации, не влияющие на flow: например, предупреждение про факты,
    /// которые могут измениться после apply предыдущих ресурсов.
    pub annotations: Vec<String>,
}

/// Сводка по плану.
#[derive(Debug, Clone, Default, Serialize)]
#[non_exhaustive]
pub struct PlanSummary {
    pub add: usize,
    pub update: usize,
    pub no_change: usize,
}

/// Отчёт о plan-only прогоне.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct PlanReport {
    pub resources: Vec<ResourcePlan>,
    pub summary: PlanSummary,
    /// Ошибки plan-фазы (например, ресурс с неизвестным kind или
    /// `PrimitiveError` при построении diff). При plan_only прогон НЕ
    /// прерывается — собираем все ошибки и возвращаем их вместе с
    /// частичным планом.
    pub errors: Vec<PlanFailure>,
}

/// Ошибка plan-фазы для конкретного ресурса.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct PlanFailure {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub message: String,
}

impl PlanReport {
    /// Есть ли pending changes — true, если хотя бы один ресурс имеет
    /// Add или Update. Используется для `--dry-run` exit-code 2.
    pub fn has_drift(&self) -> bool {
        self.summary.add > 0 || self.summary.update > 0
    }

    /// Завершился ли plan с ошибками — true, если хотя бы один ресурс
    /// не удалось распланировать.
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// Outcome одного ресурса в apply.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Outcome {
    NoChange,
    Changed,
    Failed {
        error: String,
    },
    /// Ресурс пропущен, потому что предыдущий apply упал и
    /// `continue_on_error == false`.
    Skipped,
}

/// Результат apply одного ресурса.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ResourceApplyOutcome {
    pub id: ResourceId,
    pub kind: ResourceKind,
    pub outcome: Outcome,
    pub message: String,
}

/// Сводка по apply.
#[derive(Debug, Clone, Default, Serialize)]
#[non_exhaustive]
pub struct ApplySummary {
    pub changed: usize,
    pub no_change: usize,
    pub failed: usize,
    pub skipped: usize,
}

/// Отчёт о apply прогоне.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ApplyReport {
    pub resources: Vec<ResourceApplyOutcome>,
    pub summary: ApplySummary,
}

impl ApplyReport {
    pub fn has_failures(&self) -> bool {
        self.summary.failed > 0
    }
}

/// Orchestrator владеет коллекцией примитивов и обслуживает plan/apply.
pub struct Orchestrator {
    primitives: HashMap<ResourceKind, Box<dyn Primitive>>,
}

impl Orchestrator {
    pub fn new(primitives: HashMap<ResourceKind, Box<dyn Primitive>>) -> Self {
        Self { primitives }
    }

    /// Сборка плана без apply. Не прерывает прогон при ошибке отдельного
    /// ресурса — складывает её в `PlanReport.errors`. Это даёт оператору
    /// полную картину «всё, что мы знаем», даже когда часть ресурсов
    /// сломана.
    pub fn plan_only(
        &self,
        registry: &Registry,
        facts: &dyn FactsSource,
        plan_ctx: &PlanCtx,
    ) -> Result<PlanReport, RegistryError> {
        let order = registry.topological_order()?;
        let mut resources = Vec::with_capacity(order.len());
        let mut summary = PlanSummary::default();
        let mut errors = Vec::new();

        for id in order {
            // Канонический ресурс берётся из registry по id из topo-order;
            // отсутствие — нарушение инварианта Registry, расцениваем как
            // ошибку plan-фазы.
            let Some(resource) = registry.get(&id) else {
                errors.push(PlanFailure {
                    id: id.clone(),
                    kind: ResourceKind::from_static("unknown"),
                    message: "resource missing from registry during topo iteration".to_string(),
                });
                continue;
            };
            let kind = resource.kind.clone();
            let Some(primitive) = self.primitives.get(&kind) else {
                errors.push(PlanFailure {
                    id: id.clone(),
                    kind: kind.clone(),
                    message: format!("no primitive registered for kind '{kind}'"),
                });
                continue;
            };

            match primitive.plan(resource, facts, plan_ctx) {
                Ok(diff) => {
                    match &diff {
                        Diff::NoChange => summary.no_change += 1,
                        Diff::Add { .. } => summary.add += 1,
                        Diff::Update { .. } => summary.update += 1,
                    }
                    resources.push(ResourcePlan {
                        id,
                        kind,
                        diff,
                        annotations: Vec::new(),
                    });
                }
                Err(e) => {
                    errors.push(PlanFailure {
                        id,
                        kind,
                        message: format!("{e}"),
                    });
                }
            }
        }

        Ok(PlanReport {
            resources,
            summary,
            errors,
        })
    }

    /// Per-resource sequential plan → apply с lazy dirty-tracking.
    ///
    /// `mark_dirty` — callback, вызываемый ПЕРЕД `primitive.apply`, когда
    /// `diff != NoChange`. Назначение — пометить факты, зависящие от
    /// `resource.kind`, как dirty, даже если apply упадёт. В CLI этот
    /// callback связан с `bosun_facts::FactsCollector::mark_dirty_after_apply`.
    pub fn apply(
        &self,
        registry: &Registry,
        facts: &dyn FactsSource,
        mark_dirty: &dyn Fn(&ResourceKind),
        plan_ctx: &PlanCtx,
        apply_ctx: &ApplyCtx,
        opts: ApplyOpts,
    ) -> Result<ApplyReport, RegistryError> {
        let order = registry.topological_order()?;
        let mut resources = Vec::with_capacity(order.len());
        let mut summary = ApplySummary::default();
        let mut aborted = false;

        for id in order {
            if aborted {
                // Прогон прерван предыдущим Err при continue_on_error=false.
                // Оставшиеся ресурсы помечаем Skipped — это явный сигнал
                // оператору, что часть плана не была даже распланирована.
                let kind = registry
                    .get(&id)
                    .map(|r| r.kind.clone())
                    .unwrap_or_else(|| ResourceKind::from_static("unknown"));
                resources.push(ResourceApplyOutcome {
                    id,
                    kind,
                    outcome: Outcome::Skipped,
                    message: "skipped: aborted after earlier failure".to_string(),
                });
                summary.skipped += 1;
                continue;
            }

            // Snapshot of resource + primitive lookup. Из топо-порядка id
            // обязан быть в registry; отсутствие — нарушение инварианта.
            let Some(resource) = registry.get(&id) else {
                resources.push(ResourceApplyOutcome {
                    id: id.clone(),
                    kind: ResourceKind::from_static("unknown"),
                    outcome: Outcome::Failed {
                        error: "resource missing from registry during topo iteration".to_string(),
                    },
                    message: String::new(),
                });
                summary.failed += 1;
                if !opts.continue_on_error {
                    aborted = true;
                }
                continue;
            };
            let kind = resource.kind.clone();
            let Some(primitive) = self.primitives.get(&kind) else {
                let message = format!("no primitive registered for kind '{kind}'");
                resources.push(ResourceApplyOutcome {
                    id: id.clone(),
                    kind: kind.clone(),
                    outcome: Outcome::Failed {
                        error: message.clone(),
                    },
                    message,
                });
                summary.failed += 1;
                if !opts.continue_on_error {
                    aborted = true;
                }
                continue;
            };

            // Step 1: plan.
            let diff = match primitive.plan(resource, facts, plan_ctx) {
                Ok(d) => d,
                Err(e) => {
                    let message = format!("plan failed: {e}");
                    resources.push(ResourceApplyOutcome {
                        id: id.clone(),
                        kind: kind.clone(),
                        outcome: Outcome::Failed {
                            error: message.clone(),
                        },
                        message,
                    });
                    summary.failed += 1;
                    if !opts.continue_on_error {
                        aborted = true;
                    }
                    continue;
                }
            };

            // Step 2: NoChange → выход без apply.
            if matches!(diff, Diff::NoChange) {
                resources.push(ResourceApplyOutcome {
                    id,
                    kind,
                    outcome: Outcome::NoChange,
                    message: String::new(),
                });
                summary.no_change += 1;
                continue;
            }

            // Step 3: mark_dirty ПЕРЕД apply. Это критично: при failed apply
            // факт может или не может остаться валидным — мы не знаем.
            // Помечаем dirty заранее, чтобы следующий get пересобрал.
            mark_dirty(&kind);

            // Step 4: apply.
            match primitive.apply(resource, &diff, apply_ctx) {
                Ok(report) => {
                    if report.changed {
                        resources.push(ResourceApplyOutcome {
                            id,
                            kind,
                            outcome: Outcome::Changed,
                            message: report.message,
                        });
                        summary.changed += 1;
                    } else {
                        // diff != NoChange, но primitive отчитался changed=false.
                        // Такое бывает: convergent apply увидел, что система
                        // уже в желаемом состоянии (race с другим оператором,
                        // например). Засчитываем как NoChange.
                        resources.push(ResourceApplyOutcome {
                            id,
                            kind,
                            outcome: Outcome::NoChange,
                            message: report.message,
                        });
                        summary.no_change += 1;
                    }
                }
                Err(e) => {
                    let (message, error_text) = describe_primitive_error(&e);
                    resources.push(ResourceApplyOutcome {
                        id,
                        kind,
                        outcome: Outcome::Failed { error: error_text },
                        message,
                    });
                    summary.failed += 1;
                    if !opts.continue_on_error {
                        aborted = true;
                    }
                }
            }
        }

        Ok(ApplyReport { resources, summary })
    }
}

/// Развернуть `PrimitiveError` в пару (human-readable message, structured error).
/// Для MVP оба поля совпадают — спека требует только строковую сериализацию.
fn describe_primitive_error(err: &PrimitiveError) -> (String, String) {
    let text = format!("{err}");
    (text.clone(), text)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio_util::sync::CancellationToken;

    use crate::call_args::CallArgs;
    use crate::diff::{ChangeReport, Diff};
    use crate::facts::FactValue;
    use crate::primitive::{ApplyCtx, FactsSource, PlanCtx};
    use crate::registry::Registry;
    use crate::resource::{Resource, ResourceId, ResourceKind};
    use crate::sensitive::SensitiveStore;

    use super::*;

    struct NoFacts;

    impl FactsSource for NoFacts {
        fn get(&self, name: &str) -> FactValue {
            FactValue::Unknown {
                reason: format!("no fact '{name}'"),
            }
        }
    }

    /// План-сценарий для одного ресурса: что должен вернуть plan/apply.
    /// Сценарии используются mock-примитивом по индексу вызова.
    #[derive(Clone, Debug)]
    enum PlanResult {
        NoChange,
        Add(&'static str),
    }

    #[derive(Clone, Debug)]
    enum ApplyResult {
        Ok(&'static str),
        Err(&'static str),
    }

    /// Recording-mock: фиксирует все вызовы plan/apply в общий лог
    /// (через `Rc<RefCell<Vec<...>>>`), а возвращает результаты из
    /// заранее заданных списков. Так как primitive должен быть Send + Sync,
    /// делим лог через `Arc<Mutex<...>>`. Тесты однопоточные, lock не блокирует.
    struct RecordingPrimitive {
        kind: ResourceKind,
        log: Arc<std::sync::Mutex<Vec<String>>>,
        plan_results: std::sync::Mutex<Vec<PlanResult>>,
        apply_results: std::sync::Mutex<Vec<ApplyResult>>,
    }

    impl RecordingPrimitive {
        fn new(
            kind: ResourceKind,
            log: Arc<std::sync::Mutex<Vec<String>>>,
            plan_results: Vec<PlanResult>,
            apply_results: Vec<ApplyResult>,
        ) -> Self {
            Self {
                kind,
                log,
                plan_results: std::sync::Mutex::new(plan_results),
                apply_results: std::sync::Mutex::new(apply_results),
            }
        }
    }

    impl Primitive for RecordingPrimitive {
        fn type_name(&self) -> ResourceKind {
            self.kind.clone()
        }
        fn identity_keys(&self) -> &'static [&'static str] {
            &["name"]
        }
        fn build_payload(
            &self,
            _args: &CallArgs,
            _ctx: &PlanCtx,
        ) -> Result<serde_json::Value, PrimitiveError> {
            Ok(serde_json::json!({}))
        }
        fn plan(
            &self,
            resource: &Resource,
            _facts: &dyn FactsSource,
            _ctx: &PlanCtx,
        ) -> Result<Diff, PrimitiveError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("plan:{}", resource.id));
            let result = self
                .plan_results
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(PlanResult::NoChange);
            Ok(match result {
                PlanResult::NoChange => Diff::NoChange,
                PlanResult::Add(desc) => Diff::Add {
                    description: desc.to_string(),
                    payload: serde_json::json!({}),
                },
            })
        }
        fn apply(
            &self,
            resource: &Resource,
            _diff: &Diff,
            _ctx: &ApplyCtx,
        ) -> Result<ChangeReport, PrimitiveError> {
            self.log
                .lock()
                .unwrap()
                .push(format!("apply:{}", resource.id));
            let result = self
                .apply_results
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(ApplyResult::Ok("default"));
            match result {
                ApplyResult::Ok(msg) => Ok(ChangeReport::changed(msg)),
                ApplyResult::Err(msg) => Err(PrimitiveError::InvalidPayload(msg.to_string())),
            }
        }
    }

    fn kind(name: &'static str) -> ResourceKind {
        ResourceKind::from_static(name)
    }

    fn resource(kind_str: &'static str, identity: &str, deps: Vec<ResourceId>) -> Resource {
        let k = kind(kind_str);
        Resource {
            id: ResourceId::new(&k, identity),
            kind: k,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: Vec::new(),
            depends_on: deps,
        }
    }

    fn plan_ctx() -> PlanCtx {
        PlanCtx {
            deadline: Instant::now() + Duration::from_secs(60),
            cancel: CancellationToken::new(),
        }
    }

    fn apply_ctx() -> ApplyCtx {
        ApplyCtx {
            deadline: Instant::now() + Duration::from_secs(60),
            cancel: CancellationToken::new(),
            log_span: tracing::Span::none(),
            sensitive: Arc::new(SensitiveStore::new()),
            backup_root: PathBuf::from("/tmp/test-backups"),
            log_dir: PathBuf::from("/tmp/test-logs"),
        }
    }

    #[test]
    fn plan_only_no_changes_has_no_drift() {
        let mut reg = Registry::new();
        reg.add(resource("apt.package", "nginx", vec![])).unwrap();
        reg.add(resource("apt.package", "curl", vec![])).unwrap();

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(
            kind("apt.package"),
            Box::new(RecordingPrimitive::new(
                kind("apt.package"),
                Arc::clone(&log),
                vec![PlanResult::NoChange, PlanResult::NoChange],
                vec![],
            )),
        );

        let orchestrator = Orchestrator::new(primitives);
        let report = orchestrator.plan_only(&reg, &NoFacts, &plan_ctx()).unwrap();
        assert_eq!(report.resources.len(), 2);
        assert!(!report.has_drift());
        assert_eq!(report.summary.no_change, 2);
        assert_eq!(report.summary.add, 0);
        assert!(!report.has_errors());
    }

    #[test]
    fn plan_only_with_add_has_drift() {
        let mut reg = Registry::new();
        reg.add(resource("apt.package", "nginx", vec![])).unwrap();

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(
            kind("apt.package"),
            Box::new(RecordingPrimitive::new(
                kind("apt.package"),
                Arc::clone(&log),
                vec![PlanResult::Add("install nginx")],
                vec![],
            )),
        );

        let orchestrator = Orchestrator::new(primitives);
        let report = orchestrator.plan_only(&reg, &NoFacts, &plan_ctx()).unwrap();
        assert!(report.has_drift());
        assert_eq!(report.summary.add, 1);
    }

    #[test]
    fn plan_only_unknown_primitive_collects_error_not_panics() {
        let mut reg = Registry::new();
        reg.add(resource("apt.package", "nginx", vec![])).unwrap();
        let orchestrator = Orchestrator::new(HashMap::new());
        let report = orchestrator.plan_only(&reg, &NoFacts, &plan_ctx()).unwrap();
        assert!(report.has_errors());
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].message.contains("no primitive registered"));
    }

    #[test]
    fn apply_skips_apply_for_no_change_resources() {
        let mut reg = Registry::new();
        reg.add(resource("apt.package", "nginx", vec![])).unwrap();
        reg.add(resource("apt.package", "curl", vec![])).unwrap();

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(
            kind("apt.package"),
            Box::new(RecordingPrimitive::new(
                kind("apt.package"),
                Arc::clone(&log),
                vec![PlanResult::NoChange, PlanResult::NoChange],
                vec![],
            )),
        );

        let orchestrator = Orchestrator::new(primitives);
        let dirty_log: RefCell<Vec<ResourceKind>> = RefCell::new(Vec::new());
        let mark_dirty = |k: &ResourceKind| dirty_log.borrow_mut().push(k.clone());
        let report = orchestrator
            .apply(
                &reg,
                &NoFacts,
                &mark_dirty,
                &plan_ctx(),
                &apply_ctx(),
                ApplyOpts::default(),
            )
            .unwrap();

        // 2 plan calls, 0 apply calls.
        let log_snapshot = log.lock().unwrap().clone();
        assert_eq!(
            log_snapshot
                .iter()
                .filter(|s| s.starts_with("plan:"))
                .count(),
            2
        );
        assert_eq!(
            log_snapshot
                .iter()
                .filter(|s| s.starts_with("apply:"))
                .count(),
            0
        );

        assert!(dirty_log.borrow().is_empty());
        assert_eq!(report.summary.no_change, 2);
        assert_eq!(report.summary.changed, 0);
        assert_eq!(report.summary.failed, 0);
    }

    #[test]
    fn apply_mixed_add_and_no_change_runs_apply_only_for_add() {
        let mut reg = Registry::new();
        reg.add(resource("apt.package", "nginx", vec![])).unwrap();
        reg.add(resource("apt.package", "curl", vec![])).unwrap();

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        // plan_results — стек, последний pop первый. Ставим в обратном порядке.
        // Topo с независимыми ресурсами недетерминирован, поэтому оба плана
        // дают по одному Add / NoChange.
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(
            kind("apt.package"),
            Box::new(RecordingPrimitive::new(
                kind("apt.package"),
                Arc::clone(&log),
                vec![PlanResult::NoChange, PlanResult::Add("install something")],
                vec![ApplyResult::Ok("installed")],
            )),
        );

        let orchestrator = Orchestrator::new(primitives);
        let dirty_log: RefCell<Vec<ResourceKind>> = RefCell::new(Vec::new());
        let mark_dirty = |k: &ResourceKind| dirty_log.borrow_mut().push(k.clone());
        let report = orchestrator
            .apply(
                &reg,
                &NoFacts,
                &mark_dirty,
                &plan_ctx(),
                &apply_ctx(),
                ApplyOpts::default(),
            )
            .unwrap();

        let log_snapshot = log.lock().unwrap().clone();
        assert_eq!(
            log_snapshot
                .iter()
                .filter(|s| s.starts_with("plan:"))
                .count(),
            2
        );
        assert_eq!(
            log_snapshot
                .iter()
                .filter(|s| s.starts_with("apply:"))
                .count(),
            1
        );

        // mark_dirty вызван один раз — только перед apply ресурса с Add.
        assert_eq!(dirty_log.borrow().len(), 1);
        assert_eq!(report.summary.changed, 1);
        assert_eq!(report.summary.no_change, 1);
        assert_eq!(report.summary.failed, 0);
    }

    #[test]
    fn apply_with_error_aborts_when_continue_on_error_false() {
        let mut reg = Registry::new();
        // Цепочка: nginx depends on curl, curl independent. Topo даст curl первым.
        let curl = reg.add(resource("apt.package", "curl", vec![])).unwrap();
        reg.add(resource("apt.package", "nginx", vec![curl]))
            .unwrap();

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        // plan: оба Add. apply: первый Err (curl), для второго не должен быть
        // вызван.
        // Stack pop: для curl (первый apply call) pop последний элемент Err.
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(
            kind("apt.package"),
            Box::new(RecordingPrimitive::new(
                kind("apt.package"),
                Arc::clone(&log),
                vec![PlanResult::Add("a"), PlanResult::Add("b")],
                vec![ApplyResult::Err("boom for curl")],
            )),
        );

        let orchestrator = Orchestrator::new(primitives);
        let dirty_log: RefCell<Vec<ResourceKind>> = RefCell::new(Vec::new());
        let mark_dirty = |k: &ResourceKind| dirty_log.borrow_mut().push(k.clone());
        let report = orchestrator
            .apply(
                &reg,
                &NoFacts,
                &mark_dirty,
                &plan_ctx(),
                &apply_ctx(),
                ApplyOpts {
                    continue_on_error: false,
                },
            )
            .unwrap();

        let log_snapshot = log.lock().unwrap().clone();
        // 1 plan call (curl), 1 apply call (curl, failed). nginx не плановался.
        assert_eq!(
            log_snapshot
                .iter()
                .filter(|s| s.starts_with("plan:"))
                .count(),
            1
        );
        assert_eq!(
            log_snapshot
                .iter()
                .filter(|s| s.starts_with("apply:"))
                .count(),
            1
        );

        // mark_dirty вызван — БЕЗ зависимости от исхода apply.
        assert_eq!(dirty_log.borrow().len(), 1);

        // Один failed (curl), один skipped (nginx).
        assert!(report.has_failures());
        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.skipped, 1);
    }

    #[test]
    fn apply_with_continue_on_error_processes_all_resources() {
        let mut reg = Registry::new();
        let curl = reg.add(resource("apt.package", "curl", vec![])).unwrap();
        reg.add(resource("apt.package", "nginx", vec![curl]))
            .unwrap();

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        // plan: оба Add. apply: pop порядок: первый pop = apply curl, второй pop = apply nginx.
        // Стек pop забирает с конца; ставим [Ok nginx, Err curl] чтобы pop дал Err первым.
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(
            kind("apt.package"),
            Box::new(RecordingPrimitive::new(
                kind("apt.package"),
                Arc::clone(&log),
                vec![PlanResult::Add("a"), PlanResult::Add("b")],
                vec![ApplyResult::Ok("ok nginx"), ApplyResult::Err("err curl")],
            )),
        );

        let orchestrator = Orchestrator::new(primitives);
        let dirty_log: RefCell<Vec<ResourceKind>> = RefCell::new(Vec::new());
        let mark_dirty = |k: &ResourceKind| dirty_log.borrow_mut().push(k.clone());
        let report = orchestrator
            .apply(
                &reg,
                &NoFacts,
                &mark_dirty,
                &plan_ctx(),
                &apply_ctx(),
                ApplyOpts {
                    continue_on_error: true,
                },
            )
            .unwrap();

        let log_snapshot = log.lock().unwrap().clone();
        assert_eq!(
            log_snapshot
                .iter()
                .filter(|s| s.starts_with("plan:"))
                .count(),
            2
        );
        assert_eq!(
            log_snapshot
                .iter()
                .filter(|s| s.starts_with("apply:"))
                .count(),
            2
        );

        // mark_dirty вызван дважды — перед каждым apply.
        assert_eq!(dirty_log.borrow().len(), 2);

        assert_eq!(report.summary.failed, 1);
        assert_eq!(report.summary.changed, 1);
        assert_eq!(report.summary.skipped, 0);
    }

    #[test]
    fn apply_calls_mark_dirty_before_primitive_apply() {
        // Семантика: mark_dirty(kind) должен идти ДО apply, даже если apply
        // упадёт. Проверка: в edge case error apply, mark_dirty всё равно вызван.
        let mut reg = Registry::new();
        reg.add(resource("apt.package", "nginx", vec![])).unwrap();

        // Захватываем порядок вызовов в общий лог. mark_dirty пишет
        // "mark_dirty:<kind>", primitive пишет "apply:<id>".
        let event_log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let event_log_for_primitive = Arc::clone(&event_log);

        let plog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        // Хитрый mock: при apply пишет в общий event_log плюс возвращает Err.
        struct OrderingPrimitive {
            kind: ResourceKind,
            inner_log: Arc<std::sync::Mutex<Vec<String>>>,
            event_log: Arc<std::sync::Mutex<Vec<String>>>,
        }
        impl Primitive for OrderingPrimitive {
            fn type_name(&self) -> ResourceKind {
                self.kind.clone()
            }
            fn identity_keys(&self) -> &'static [&'static str] {
                &["name"]
            }
            fn build_payload(
                &self,
                _args: &CallArgs,
                _ctx: &PlanCtx,
            ) -> Result<serde_json::Value, PrimitiveError> {
                Ok(serde_json::json!({}))
            }
            fn plan(
                &self,
                resource: &Resource,
                _facts: &dyn FactsSource,
                _ctx: &PlanCtx,
            ) -> Result<Diff, PrimitiveError> {
                self.inner_log
                    .lock()
                    .unwrap()
                    .push(format!("plan:{}", resource.id));
                Ok(Diff::Add {
                    description: "x".to_string(),
                    payload: serde_json::json!({}),
                })
            }
            fn apply(
                &self,
                _resource: &Resource,
                _diff: &Diff,
                _ctx: &ApplyCtx,
            ) -> Result<ChangeReport, PrimitiveError> {
                self.event_log.lock().unwrap().push("apply".to_string());
                Err(PrimitiveError::InvalidPayload("forced fail".to_string()))
            }
        }
        primitives.insert(
            kind("apt.package"),
            Box::new(OrderingPrimitive {
                kind: kind("apt.package"),
                inner_log: Arc::clone(&plog),
                event_log: Arc::clone(&event_log_for_primitive),
            }),
        );

        let orchestrator = Orchestrator::new(primitives);
        let event_log_for_dirty = Arc::clone(&event_log);
        let mark_dirty = |k: &ResourceKind| {
            event_log_for_dirty
                .lock()
                .unwrap()
                .push(format!("mark_dirty:{k}"));
        };

        let report = orchestrator
            .apply(
                &reg,
                &NoFacts,
                &mark_dirty,
                &plan_ctx(),
                &apply_ctx(),
                ApplyOpts::default(),
            )
            .unwrap();

        let events = event_log.lock().unwrap().clone();
        // mark_dirty:apt.package, apply
        assert_eq!(events.len(), 2);
        assert!(events[0].starts_with("mark_dirty:"));
        assert_eq!(events[1], "apply");
        assert_eq!(report.summary.failed, 1);
    }

    #[test]
    fn apply_respects_topological_order() {
        let mut reg = Registry::new();
        // a → b → c (a первый в топо-порядке).
        let a = reg.add(resource("apt.package", "a", vec![])).unwrap();
        let b = reg
            .add(resource("apt.package", "b", vec![a.clone()]))
            .unwrap();
        reg.add(resource("apt.package", "c", vec![b])).unwrap();

        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut primitives: HashMap<ResourceKind, Box<dyn Primitive>> = HashMap::new();
        primitives.insert(
            kind("apt.package"),
            Box::new(RecordingPrimitive::new(
                kind("apt.package"),
                Arc::clone(&log),
                vec![
                    PlanResult::NoChange,
                    PlanResult::NoChange,
                    PlanResult::NoChange,
                ],
                vec![],
            )),
        );

        let orchestrator = Orchestrator::new(primitives);
        let mark_dirty = |_: &ResourceKind| {};
        orchestrator
            .apply(
                &reg,
                &NoFacts,
                &mark_dirty,
                &plan_ctx(),
                &apply_ctx(),
                ApplyOpts::default(),
            )
            .unwrap();

        let log_snapshot = log.lock().unwrap().clone();
        let plan_ids: Vec<&String> = log_snapshot
            .iter()
            .filter(|s| s.starts_with("plan:"))
            .collect();
        assert_eq!(plan_ids[0], "plan:apt.package:a");
        assert_eq!(plan_ids[1], "plan:apt.package:b");
        assert_eq!(plan_ids[2], "plan:apt.package:c");
    }

    #[test]
    fn plan_only_returns_cycle_error() {
        let ka = kind("apt.package");
        let mut reg = Registry::new();
        let id_a = ResourceId::new(&ka, "a");
        let id_b = ResourceId::new(&ka, "b");
        reg.add(Resource {
            id: id_a.clone(),
            kind: ka.clone(),
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: Vec::new(),
            depends_on: vec![id_b.clone()],
        })
        .unwrap();
        reg.add(Resource {
            id: id_b,
            kind: ka,
            spec_version: 1,
            payload: serde_json::json!({}),
            reload_on: Vec::new(),
            depends_on: vec![id_a],
        })
        .unwrap();

        let orchestrator = Orchestrator::new(HashMap::new());
        let err = orchestrator
            .plan_only(&reg, &NoFacts, &plan_ctx())
            .unwrap_err();
        assert!(matches!(err, RegistryError::Cycle { .. }));
    }
}
