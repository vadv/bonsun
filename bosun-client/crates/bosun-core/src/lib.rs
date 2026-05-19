//! bosun-core — контракты и evaluator для bosun-client.

pub mod bundle;
pub mod call_args;
pub mod defers;
pub mod diff;
pub mod digest;
pub mod evaluator;
pub mod facts;
pub mod health_check;
pub mod inventory;
pub mod orchestrator;
pub mod path_safety;
pub mod primitive;
pub mod registry;
pub mod resource;
pub mod sensitive;
pub mod starlark_glue;
pub mod unit_name;
pub mod validate;
// Тестовая утилита для tracing-recording. Не должна попадать в
// production-сборку, но крайне нужна в `#[cfg(test)]` соседних крейтов
// — там примитивы прогоняют apply под per-thread recorder'ом. Поэтому
// модуль виден всегда, но прямого API там нет — только тесты импортируют.
#[doc(hidden)]
pub mod tracing_test_util;

pub use bundle::{Bundle, BundleError, BundleInventoryConfig, BundleMetadata};
pub use call_args::{ArgValue, CallArgs, CallArgsError};
pub use defers::HealthCheck;
pub use diff::{ChangeReport, Diff};
pub use digest::sha256_hex;
pub use evaluator::Evaluator;
pub use facts::{FactCategory, FactValue, RefreshPolicy};
pub use health_check::{
    cancellable_sleep, resolve_defaults as resolve_health_check_defaults, HealthCheckError,
    HealthCheckRunner, NoopHealthCheckRunner, DEFAULT_RETRY_COUNT, DEFAULT_RETRY_INTERVAL_SEC,
    DEFAULT_TIMEOUT_SEC, EXCERPT_LIMIT as HEALTH_CHECK_EXCERPT_LIMIT,
};
pub use inventory::{
    merge_inventory, merge_inventory_keyed, InventoryError, InventorySource, JsonInventory,
    MergeStrategy,
};
pub use orchestrator::{
    ApplyOpts, ApplyReport, ApplySummary, Orchestrator, Outcome, PlanFailure, PlanReport,
    PlanSummary, ResourceApplyOutcome, ResourcePlan,
};
pub use path_safety::{resolve_within_root, PathSafetyError};
pub use primitive::{ApplyCtx, FactsSource, PlanCtx, Primitive, PrimitiveError};
pub use registry::{Registry, RegistryError};
pub use resource::{Handle, Resource, ResourceId, ResourceKind, ResourceKindError};
pub use sensitive::{SensitivePayload, SensitiveStore};
pub use starlark_glue::{
    default_template_fn, evaluate_manifest, EvaluatorConfig, StarlarkGlueError, TemplateFn,
};
pub use unit_name::{UnitName, UnitNameError, UNIT_NAME_MAX_BYTES};
pub use validate::{
    substitute_new_path, RealValidateRunner, ValidateError, ValidateRunner, STDERR_EXCERPT_LIMIT,
};
