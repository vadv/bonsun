//! bosun-core — контракты и evaluator для bosun-client.

pub mod call_args;
pub mod diff;
pub mod facts;
pub mod inventory;
pub mod primitive;
pub mod registry;
pub mod resource;
pub mod sensitive;

pub use call_args::{ArgValue, CallArgs, CallArgsError};
pub use diff::{ChangeReport, Diff};
pub use facts::{FactCategory, FactValue, RefreshPolicy};
pub use inventory::{InventoryError, InventorySource, JsonInventory};
pub use primitive::{ApplyCtx, FactsSource, PlanCtx, Primitive, PrimitiveError};
pub use registry::{Registry, RegistryError};
pub use resource::{Handle, Resource, ResourceId, ResourceKind, ResourceKindError};
pub use sensitive::{SensitivePayload, SensitiveStore};
