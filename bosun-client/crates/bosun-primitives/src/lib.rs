//! bosun-primitives — реализации trait Primitive: apt.package, file.content, template().

pub mod file_content;
pub mod template;

pub use file_content::{sha256_hex, FileContentSpec, FilePrimitive};
pub use template::{render_template, TemplateError};
