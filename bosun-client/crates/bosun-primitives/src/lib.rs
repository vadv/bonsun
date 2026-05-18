//! bosun-primitives — реализации trait Primitive: apt.package, file.content, template().

pub mod apt_package;
pub mod file_content;
pub mod template;

pub use apt_package::{AptPackageSpec, AptPrimitive};
pub use file_content::{sha256_hex, FileContentSpec, FilePrimitive};
pub use template::{render_template, TemplateError};
