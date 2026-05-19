//! bosun-primitives — реализации trait Primitive: apt.package, file.content,
//! template(), Phase D набор runr-примитивов
//! (`runr.service`/`runr.timer`/`runr.cgroup`), Phase E набор
//! systemd-примитивов (`systemd.service`/`systemd.timer`), Phase G
//! `process.signal`, Phase M `users.user`/`users.group` и Phase N
//! `file.delete`/`file.symlink` (+ `apt.package` state=absent/purged).

pub mod apt_package;
pub mod cert_tls;
pub mod dispatch;
pub mod file_content;
pub mod file_delete;
pub mod file_symlink;
pub mod health_check;
pub mod process_signal;
pub mod runr_cgroup;
pub mod runr_service;
pub mod runr_timer;
pub mod systemd_service;
pub mod systemd_timer;
pub mod template;
pub mod users_group;
pub mod users_user;

pub use apt_package::{AptPackageSpec, AptPackageState, AptPrimitive};
pub use cert_tls::{
    decide_action_cert, Action as CertTlsAction, CertAlgorithm, CertTlsPrimitive, CertTlsSpec,
};
pub use dispatch::RealDispatchClient;
pub use file_content::{sha256_hex, FileContentSpec, FilePrimitive};
pub use file_delete::{
    decide_action_delete, Action as FileDeleteAction, FileDeletePrimitive, FileDeleteSpec,
};
pub use file_symlink::{
    decide_action_symlink, Action as FileSymlinkAction, FileSymlinkPrimitive, FileSymlinkSpec,
    SymlinkState,
};
pub use health_check::RealHealthCheckRunner;
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
pub use users_group::{
    decide_action_group, Action as UsersGroupAction, GroupAddOpts, GroupInfo, GroupModOpts,
    GroupPrimitive, GroupSpec, GroupState,
};
pub use users_user::{
    decide_action_user, Action as UsersUserAction, FieldDiff as UsersUserFieldDiff,
    RealUsersBackend, UserAddOpts, UserInfo, UserModOpts, UserPrimitive, UserSpec, UserState,
    UsersBackend, UsersError,
};
