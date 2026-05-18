//! Шаблоны bundle'а: `template(path)` функция и `render_template` core.
//!
//! `render_template` — чистая функция, никаких сайд-эффектов. Принимает inv
//! и facts отдельно, потому что `FactsSource` через trait не отдаёт списка
//! имён — материализация facts в JSON-map делается на стороне caller'а (CLI
//! строит её из catalog).

mod render;

pub use render::{render_template, TemplateError};
