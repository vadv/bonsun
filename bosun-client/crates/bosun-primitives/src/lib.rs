//! bosun-primitives — реализации trait Primitive: apt.package, file.content,
//! template(), а также Phase D набор runr-примитивов:
//! `runr.service` / `runr.timer` / `runr.cgroup`.

pub mod apt_package;
pub mod file_content;
pub mod runr_cgroup;
pub mod runr_service;
pub mod runr_timer;
pub mod template;

pub use apt_package::{AptPackageSpec, AptPrimitive};
pub use file_content::{sha256_hex, FileContentSpec, FilePrimitive};
pub use runr_cgroup::{CgroupState, RunrCgroupPrimitive, RunrCgroupSpec};
pub use runr_service::{
    decide_action_runr, Action, RunrServicePrimitive, RunrServiceSpec, ServiceState,
};
pub use runr_timer::{
    decide_timer_action, RunrTimerPrimitive, RunrTimerSpec, TimerAction, TimerState,
};
pub use template::{render_template, TemplateError};
