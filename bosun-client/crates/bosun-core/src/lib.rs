//! bosun-core — контракты и evaluator для bosun-client.

pub mod diff;
pub mod resource;

pub use diff::{ChangeReport, Diff};
pub use resource::{Handle, Resource, ResourceId, ResourceKind, ResourceKindError};
