//! bosun-primitives — реализации trait Primitive: apt.package, file.content,
//! template(), Phase D набор runr-примитивов
//! (`runr.service`/`runr.timer`/`runr.cgroup`), Phase E набор
//! systemd-примитивов (`systemd.service`/`systemd.timer`) и Phase G
//! `process.signal`.

pub mod apt_package;
pub mod file_content;
pub mod process_signal;
pub mod runr_cgroup;
pub mod runr_service;
pub mod runr_timer;
pub mod systemd_service;
pub mod systemd_timer;
pub mod template;

pub use apt_package::{AptPackageSpec, AptPrimitive};
pub use file_content::{sha256_hex, FileContentSpec, FilePrimitive};
pub use process_signal::{
    build_signal_argv, ProcessSignalPrimitive, ProcessSignalRunner, ProcessSignalSpec,
    RealProcessSignalRunner,
};
pub use runr_cgroup::{CgroupState, RunrCgroupPrimitive, RunrCgroupSpec};
pub use runr_service::{
    decide_action_runr, Action, RunrServicePrimitive, RunrServiceSpec, ServiceState,
};
pub use runr_timer::{
    decide_timer_action, RunrTimerPrimitive, RunrTimerSpec, TimerAction, TimerState,
};
pub use systemd_service::{
    decide_action_systemd, Action as SystemdAction, ServiceState as SystemdServiceState,
    SystemdServicePrimitive, SystemdServiceSpec,
};
pub use systemd_timer::{
    decide_timer_action as decide_systemd_timer_action, SystemdTimerPrimitive, SystemdTimerSpec,
    TimerAction as SystemdTimerAction, TimerState as SystemdTimerState,
};
pub use template::{render_template, TemplateError};
