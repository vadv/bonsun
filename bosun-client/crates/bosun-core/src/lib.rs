//! bosun-core — контракты и evaluator для bosun-client.

pub mod bundle;
pub mod call_args;
pub mod diff;
pub mod digest;
pub mod evaluator;
pub mod facts;
pub mod inventory;
pub mod orchestrator;
pub mod primitive;
pub mod registry;
pub mod resource;
pub mod sensitive;
pub mod starlark_glue;
// Тестовая утилита для tracing-recording. Не должна попадать в
// production-сборку, но крайне нужна в `#[cfg(test)]` соседних крейтов
// — там примитивы прогоняют apply под per-thread recorder'ом. Поэтому
// модуль виден всегда, но прямого API там нет — только тесты импортируют.
#[doc(hidden)]
pub mod tracing_test_util;

pub use bundle::{Bundle, BundleError, BundleMetadata};
pub use call_args::{ArgValue, CallArgs, CallArgsError};
pub use diff::{ChangeReport, Diff};
pub use digest::sha256_hex;
pub use evaluator::Evaluator;
pub use facts::{FactCategory, FactValue, RefreshPolicy};
pub use inventory::{InventoryError, InventorySource, JsonInventory};
pub use orchestrator::{
    ApplyOpts, ApplyReport, ApplySummary, Orchestrator, Outcome, PlanFailure, PlanReport,
    PlanSummary, ResourceApplyOutcome, ResourcePlan,
};
pub use primitive::{ApplyCtx, FactsSource, PlanCtx, Primitive, PrimitiveError};
pub use registry::{Registry, RegistryError};
pub use resource::{Handle, Resource, ResourceId, ResourceKind, ResourceKindError};
pub use sensitive::{SensitivePayload, SensitiveStore};
pub use starlark_glue::{default_template_fn, evaluate_manifest, StarlarkGlueError, TemplateFn};
