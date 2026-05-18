//! bosun-core — контракты и evaluator для bosun-client.
//!
//! Этот крейт ничего не знает про конкретные примитивы (apt/file/template)
//! и про конкретные факты. Его задача — определить контракты и реализовать
//! Starlark-evaluator, Registry, plan/apply-оркестратор.
