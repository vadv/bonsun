//! bosun-facts — подсистема сбора фактов о ноде.
//!
//! Архитектурный обзор:
//! - `Fact` — trait одного факта; реализуется отдельными коллекторами.
//! - `FactsCollector` — оркестратор: владеет фактами, держит RefCell-кэш,
//!   собирает `AtStart` факты на старте, помечает `AfterApply` факты dirty.
//! - `FactsSnapshot` — иммутабельный снимок для Starlark-evaluation.
//! - `FactsView<'a>` — read-only вью, lazy-пересобирает dirty факты.

pub mod collector;

pub use collector::{Fact, FactCollectCtx, FactsCollector, FactsSnapshot, FactsView};
