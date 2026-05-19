# Runr + systemd + deferred-restart research

Date: 2026-05-19
Scope: design inputs for the next bosun iteration — runr/systemd init-system integration and a notify-driven deferred-restart model that survives crashes.

## 1. postgres-chiit defers — current implementation

Code lives in `/home/vadv/Projects/bosun/postgres-chiit/lib/defers/`. The journal is intentionally minimal — and that minimalism is the architectural lesson.

### Wire format

There is no JSON, no SQLite, no append-only log. The journal is a flat directory of executable shell scripts.

- Directory: `/tmp/chiit-defers/`, mode 0750 (see `defers/process.go:14`).
- Each pending action is a file: `<command>-<service>.sh` (see `defers/common.go:8`).
- File mode 0700, content is exactly:
  ```
  #!/bin/sh
  set -ex
  /usr/bin/systemctl daemon-reload
  /usr/bin/systemctl restart <service>
  ```
  …or the same with `/usr/bin/runr` and `reload` (`defers/runr.go:81`, `defers/systemd.go:84`).
- Generic `defers.AddCommand(name, command)` allows arbitrary scripts (used for `pkill -HUP pg_doorman`, `pkill -SIGHUP -u postgres`).

The dedup key is the **filename**. Writing `restart-nginx.sh` twice yields one file. The test `at_double_add_text_does_not_change` (`defers_test.go:36`) asserts content-stability after duplicate writes.

### Atomicity

`defers/custom.go:35` does the right thing:

1. Create `<dir>/<name>.sh_tmp_<unix-ts>` with O_RDWR|O_CREATE|O_TRUNC, mode 0700.
2. Write `#!/bin/sh\nset -ex\n<command>`.
3. `fd.Sync()` to fsync the file.
4. `fd.Close()`.
5. `os.Rename(tmp, final)` — POSIX atomic rename.

The directory itself is not fsynced after rename — a kernel/fs crash between rename and the next sync could in theory lose the file. In practice on ext4 with default `data=ordered` this is small risk; on `data=writeback` or eatmydata environments it's real. **Issue surfaced for the design doc.**

### When entries are added vs cleared

Entries are added at the time of the change, not at end of converge:
```go
files.Template(ctx, dest, tmpl, vars).Notify(func() {
    defers.AddRestartSystemd(ctx, "systemd-journald")  // synchronous
})
```
`.Notify(f)` runs `f` immediately if `Changed=true` (see `chiit/lib/handlers/types.go:28`). The file appears on disk before the rest of the role even runs.

Entries are cleared by `defers.ProcessPermanent` (`defers/process.go:19`), which runs:
- At the start of `chiit run` — "запускаем то что было потеряно в предыдущий раз" (`cmd/run.go:61`).
- At the end of `chiit run` via `defer` (`cmd/run.go:90`), inside the panic-recovery scope.

For each script: try open RW (sanity check writability) → close → execute via `/bin/sh` → `os.RemoveAll`. If execution fails, log+continue (not abort), file stays on disk for next run.

### Replay semantics

This is the persistence guarantee: **a script that fails or is interrupted between write and execute will be retried on the next `chiit run`**. The script self-documents its intent (it's literally the command). There's no metadata format to break.

Failure modes:
- File written but bosun crashes before `ProcessPermanent` runs → next run replays it. **Correct.**
- File executed but `os.RemoveAll` fails (rare) → next run replays it. **Idempotent because `systemctl restart` is idempotent — but `systemctl daemon-reload && restart` does a real restart every time it runs.**
- Corrupt script (truncated mid-write) → `set -e` makes shell abort early; file stays; next run replays. The atomic rename prevents this anyway.
- `/tmp` cleaned between reboots → **defers are lost across reboots.** This is a real bug: `/tmp` may be `tmpfs` on some systems. The directory should be under `/var/lib/`.

### What "restart" can be

The `addCommand` primitive accepts arbitrary shell. Found usages:
- `defers.AddRestartSystemd(ctx, name)`, `AddReloadSystemd`, `AddRestartRunr`, `AddReloadRunr` — typed wrappers.
- `defers.AddRestartMayBeRunrOrSystemd(ctx, name, inv)` — dispatches by init system (`defers/runr.go:17`).
- `defers.AddCommand(ctx, "blind-reload-postgresql-via-sighup", "pkill --signal SIGHUP -u postgres")` (`roles/postgres_manage/init_config.go:87`).
- `defers.AddCommand(ctx, "hup-pg-doorman", "pkill -HUP pg_doorman")` (`roles/pooler/install_pg_doorman.go:315`).

The "command" abstraction is therefore not "restart a unit" — it is "run a named script idempotently after the rest of the converge". The name is the dedup key.

### Conversion vs restart

There's also a *manual* dedup rule encoded in pairwise wrappers: `AddRestart*` calls `RemoveReload*` (`defers/systemd.go:25`), and `AddReload*` is a no-op when a restart for the same service already exists (`defers/systemd.go:52`). Restart strictly subsumes reload. Implemented by checking for the sibling file on disk. Tested by `defers_test.go`.

## 2. Industry survey

Matrix: tool / notify model / dedup / persistence / failure mode / notes.

| Tool | Notify mechanism | Dedup | Persistence | If run aborts |
|---|---|---|---|---|
| Chef | `notifies :restart, 'service[x]', :delayed` / `:immediately` / `:before`; reverse: `subscribes` | `:delayed` dedups by (resource, action) pair; `:immediately` does not dedup | In-memory queue inside chef-client run | `:delayed` notifications **still fire** by default — this is a documented hazard ([chef/chef#2084](https://github.com/chef/chef/issues/2084)); proposed `:only_on_success` flag never landed as default |
| Ansible | `notify: "restart x"` on task; handler block; `meta: flush_handlers` for explicit flush; `listen:` for topic-style | Dedups by handler name; multiple tasks notifying same handler = one execution | None — in-memory only ([2.17 listen-in-collection memory bug](https://github.com/ansible/ansible/issues/83392)) | Handlers **skipped** by default; `force_handlers: true` makes them fire after failure |
| Puppet | `notify`/`subscribe` metaparameters; resource `refresh` action; auto-relationships via `before`/`require` | `refresh` action runs **at most once per run per resource** | None — catalog is rebuilt each run | If a dependency fails, dependent resource is skipped, but refresh events from successful dependencies still propagate to it |
| Salt | `watch_in` / `watch` / `mod_watch` function on a state | `mod_watch` runs once per state per run | None — pillar/state evaluation each run | If `mod_watch` is missing, `watch` degrades to `require` |
| chiit (current) | `.Notify(func() { defers.AddX(...) })` callbacks | Dedup by filename in `/tmp/chiit-defers/` | **On-disk shell scripts**, replayed next run | Survives mid-run crash and reboot of process (but lost if `/tmp` is wiped) |

Sources:
- [Chef notifies/subscribes reference](https://www.rubydoc.info/gems/chef/Chef/Resource:notifies)
- [Chef issue: delayed notifications fire after failure](https://github.com/chef/chef/issues/2084)
- [Ansible handlers doc](https://docs.ansible.com/projects/ansible/latest/playbook_guide/playbooks_handlers.html)
- [Ansible meta module](https://docs.ansible.com/projects/ansible/latest/collections/ansible/builtin/meta_module.html)
- [Puppet relationships and ordering](https://www.puppet.com/docs/puppet/7/lang_relationships.html)
- [Salt requisites](https://docs.saltproject.io/en/latest/ref/states/requisites.html)
- [SaltStack mod_watch issue](https://github.com/saltstack/salt/issues/2035)

### Concrete observations for bosun

- **Persistence is rare.** Of the four industry tools, none persists pending handlers across process death. chiit is unusual.
- **Dedup by name is universal.** Chef, Ansible, Puppet, Salt all dedup by some form of "handler identity"; chiit's "filename as identity" is in this family.
- **Ordering varies.** Ansible runs handlers in their definition order. Chef runs delayed notifications in update order. Puppet's refresh is a graph traversal. chiit's defers run in `sort.Strings(list)` order — i.e. ASCII filename order, which is mostly stable across runs.
- **Failure semantics is the contentious knob.** Chef's default ("fire delayed even after failure") is the most controversial. Ansible's default ("skip on failure unless force_handlers") is the safer default. Persistent journals like chiit's behave like Chef's default with one extra superpower: the journal *itself* persists across the failure, so the recovery happens on next run rather than being smuggled in at the end of the failing run.

## 3. systemd dbus surface

### Interface: `org.freedesktop.systemd1.Manager`

Methods relevant to bosun (see [org.freedesktop.systemd1(5)](https://man7.org/linux/man-pages/man5/org.freedesktop.systemd1.5.html)):

```
RestartUnit(in s name, in s mode, out o job)
ReloadUnit(in s name, in s mode, out o job)
ReloadOrRestartUnit(in s name, in s mode, out o job)
StartUnit(in s name, in s mode, out o job)
StopUnit(in s name, in s mode, out o job)
EnableUnitFiles(in as files, in b runtime, in b force, out b carries_install_info, out a(sss) changes)
DisableUnitFiles(in as files, in b runtime, out a(sss) changes)
Reload()  // daemon-reload
GetUnit(in s name, out o unit_path)
ListUnits(out a(ssssssouso) units)
ListUnitFilesByPatterns(in as states, in as patterns, out a(ss) files)
```

`mode` is one of `replace`, `fail`, `isolate`, `ignore-dependencies`, `ignore-requirements`. chiit uses `"replace"` (see `manager_dbus.go:168`) — start a new job, replacing any pending conflicting job.

### Signals

```
JobNew(u id, o job, s unit)
JobRemoved(u id, o job, s unit, s result)
```

`result` is `"done" | "canceled" | "timeout" | "failed" | "dependency" | "skipped"`. **This is the verification primitive.** `RestartUnit` returns a job object path; the actual restart is async. To know whether the restart succeeded, subscribe to `JobRemoved` and match on the returned object path.

Caveat: [Debian bug 996911](https://www.mail-archive.com/pkg-systemd-maintainers@alioth-lists.debian.net/msg06727.html) documents that `JobRemoved` may report `"done"` even when the unit subsequently failed to start (the job-system result and the unit-execution result are not the same thing). Verification needs a second step: query `ActiveState` and `SubState` on the unit afterwards.

### Properties for verification

On `org.freedesktop.systemd1.Unit`:
- `ActiveState`: `"active" | "reloading" | "inactive" | "failed" | "activating" | "deactivating"`.
- `SubState`: `"running" | "exited" | "dead" | ...` — unit-type-specific.
- `ActiveEnterTimestamp` (type `t`, microseconds since epoch): wall-clock instant of last entry into active.
- `InvocationID` (type `ay`): per-invocation 128-bit UUID. Changes on every (re)start. **This is the cleanest dedup primitive: snapshot `InvocationID` before restart, compare after.**

On `org.freedesktop.systemd1.Service` (via `BUS_EXEC_STATUS_VTABLE` in [`src/core/dbus-service.c`](https://github.com/systemd/systemd/blob/main/src/core/dbus-service.c)):
- `ExecMainStartTimestamp` — when the main ExecStart= process began.
- `ExecMainStartTimestampMonotonic` — same, monotonic clock (immune to clock jumps).
- `ExecMainPID`.

For a definitive "did this restart actually run" check, compare `InvocationID` before and after the operation. `InvocationID` is also exposed in journald (`_SYSTEMD_INVOCATION_ID=`), so end-to-end correlation is possible.

### Crate choice

Two viable options:
- **zbus 5.x** ([lib.rs/zbus](https://lib.rs/crates/zbus)) — pure-Rust, async-first, requires choosing async runtime (`tokio` vs `async-std` feature). Has `zbus-blocking` shim for sync contexts. Modern community default.
- **zbus_systemd** ([docs.rs/zbus_systemd](https://docs.rs/zbus_systemd)) — built on zbus, ships pre-generated systemd1/login1/hostname1 proxies. Includes `ManagerProxy::receive_job_removed` for the signal stream. Pre-generated `RestartUnitContext` etc. — no XML proxy macro toil.

**Recommendation: zbus_systemd** for the Manager surface (saves writing the proxy declarations) plus raw zbus for anything custom. tokio runtime since bosun's TLA already implies an async-first body for parallel facts.

### Failure modes

- **Bus availability**: must be the *system* bus. `Connection::system().await` in zbus. Inside a container without `/run/dbus/system_bus_socket` mounted, this fails immediately — bosun must detect and either fall back to runr or refuse.
- **Authorization**: PID 1 (systemd) requires `org.freedesktop.systemd1.manage-units` polkit action for state-changing methods. Bosun running as root → granted unconditionally. Bosun running as non-root → polkit rule in `/etc/polkit-1/rules.d/` ([Arch wiki polkit](https://wiki.archlinux.org/title/Polkit)). For initial design assume root.
- **Async job completion**: the dbus call returns when the job is queued, not when it completes. Without subscribing to `JobRemoved` the caller has no idea whether the restart actually succeeded. chiit's current code calls `RestartUnitContext` and treats `err == nil` as `Changed=true` — this is wrong for any unit that takes meaningful time to start.
- **Unit-not-found**: GetUnit returns `org.freedesktop.systemd1.NoSuchUnit`. Worth a typed error in the bosun layer.

## 4. runr full API

Source of truth: `/home/vadv/Projects/bosun/postgres-chiit/lib/runr/`.

### HTTP endpoints (from `client.go`)

```
GET  /api/v1/daemon/info               -> DaemonInfo
POST /api/v1/units/reload              -> ActionAck  (daemon-reload)
POST /api/v1/services/{name}/start     -> ActionAck  body: { idempotent: bool }
POST /api/v1/services/{name}/stop      -> ActionAck  body: { timeout?, force }
POST /api/v1/services/{name}/restart   -> ActionAck  body: { stop: StopOptions, start: StartOptions }
POST /api/v1/services/{name}/reload    -> ActionAck
POST /api/v1/timers/{name}/start       -> ActionAck
POST /api/v1/timers/{name}/stop        -> ActionAck
POST /api/v1/timers/{name}/enable      -> ActionAck  body: { now: bool }
POST /api/v1/timers/{name}/disable     -> ActionAck  body: { now: bool }
GET  /api/v1/units                     -> [UnitListItem]
GET  /api/v1/services/statuses         -> [ServiceStatus]
GET  /api/v1/timers/statuses           -> [TimerStatus]
```

Base URL hardcoded to `http://127.0.0.1:8010` (`runr/chiit.go:19`). `ActionAck` returns an `action_id` and `accepted_at` — but there is no follow-up endpoint to poll the action's terminal state. Status comes from `/api/v1/services/statuses` which gives `State`, `Restarts`, `InStateForMs`. This is weaker than systemd's `JobRemoved` and `InvocationID` — for bosun we'll have to compare `Restarts` before and after, or compare `StartedAt`.

### INI format for unit files

Lives in `/etc/runr/` — directly analogous to `/etc/systemd/system/`. Three file types:
- `*.service` — `[Service]` and optional `[Log]` sections.
- `*.timer` — `[Timer]` section.
- `*.cgroup` — bare config for cgroup v2 limits.

`[Service]` fields (from `runr/types.go`): `Type`, `User`, `Group`, `WorkingDirectory`, `Environment`, `EnvironmentFile`, `ExecStart`, `ExecStartPre`, `ExecReload`, `ExecStop`, `Restart` (`no|on-failure|always|halt`), `TimeoutStartSec`, `TimeoutStopSec`, `RestartSec`, `Nice`, `LimitNOFILE`, `CapabilityBoundingSet`, `Autostart` (runr-specific, not systemd), `MaxMemoryRSS` (runr-specific programmatic OOM), `PIDFile`, `KillMode`, `CgroupProcsPath` (runr-init only).

`[Log]` is **runr-only**: `Directory`, `FileSizeBytes`, `FileCount`, `RotateEvery`, `Sink` (`none|stdout|stderr`), `Prefix`, `PrefixFile`, `PrefixSink`. Templates `%T`, `%d`, `%I`, `%U`, `%p`, `%S`, `%R`, `%s`.

`[Timer]`: `OnCalendar`, `OnStartupSec`, `OnUnitInactiveSec`, `RandomizedDelaySec`, `Unit`, `Autostart`.

`*.cgroup`: `Path`, `MemoryMax`, `CPUMax` (percent: 250.0 → 2.5 CPU), `IOMax` (per-mountpoint read_bps/write_bps/read_iops/write_iops).

### runr-init mode

When runr runs as PID 1 in a container, three symlinks are created (`runr/chiit.go:64`):
```
/usr/bin/systemctl -> /usr/bin/runr
/bin/systemctl     -> /usr/bin/runr
/usr/bin/journalctl -> /usr/bin/runr
/usr/bin/systemd-run -> /usr/bin/runr
```
runr inspects argv[0] and dispatches. This is how chiit-the-old can keep calling `systemctl restart foo` inside a pod and have it work.

The `template_functions.IsRunRInit()` predicate gates many code paths:
- In runr-init mode, `defers.AddRestartSystemd` panics (`defers/systemd.go:17`).
- `systemd.SetSystemdManager` switches the default from dbus-backed to shell-backed when `IsRunRInit()` (which means the shell `systemctl` is actually the runr shim).
- `runr/chiit.go:73-83` writes additional Prometheus exporters: `runr_init` and `runr_init_version` metrics.

### Conversion: systemd → runr

`runr/systemd.go` exposes `ServiceFromSystemd(*systemd.ServiceConfig) runr.Service` and `TimerFromSystemd`. Translates:
- `Restart=on-success` → `RestartPolicy::Always` (no runr equivalent for on-success specifically).
- `TimeoutStartSec`/`TimeoutStopSec` in seconds → humantime `"90s"` string.
- `MaxMemoryMb` (MiB) → `MaxMemoryRSS` (bytes, ×1024²).
- `SecureSettings.CapabilityBoundingSet` (space-joined string) → array.
- `StandardOutputTTY` (bool) → `[Log] Sink="stdout"` (otherwise `Directory=/var/log/<unit>`).

The current converter is one-way (systemd-config → runr-INI). For bosun we'd want both directions, but more importantly: bosun's primitive should accept a single declarative schema and emit *either* `/etc/systemd/system/foo.service` *or* `/etc/runr/foo.service` based on fact `init_system`.

## 5. postgres-chiit role inventory

| Role | Primary resources | Services touched (notify target) | Special primitives |
|---|---|---|---|
| chaosd | apt.Install, runr.NewService | runr `chaosd` | — |
| chiit | systemd.NewTimerAndStarted, runr.NewTimerAndStarted | chiit timers | force-timer logic |
| copycatp2s | systemd.NewServiceAndStarted, runr.NewServiceAndStarted | `copycat-p2s` | schedule logic |
| hosts | files.Create only | (no defers — overwrites /etc/hosts) | localhost canon insertion |
| journald | files.Template | restart systemd-journald | — |
| logrotate | files.Template | restart cron | — |
| patroni | apt.Install, files.Template, systemd.NewServiceAndStarted | `patroni`, `patroni-guard` (runr or systemd dispatch) | renice, guard, start orchestration |
| pg_mediator | files.Template, cron, command.New | (no service — just files+cron) | archive_command / restore_command rendering |
| pod | files.Template, liveness check | — | runr-init pod-specific config (net.go, memory.go, io.go) |
| pooler | apt.Install, files.Template, runr/systemd dispatch, **files.TemplateWithValidation** | `pg_doorman`, `pg_doorman_users`, `janus-client` (restart and reload variants), `pkill -HUP pg_doorman` | **validation hook** before reload |
| postgres | apt.Install (PostgreSQL pkgs), files.Template, command.New | `ssh` (reload), pg_probackup files | safe_scripts, silence_host, init_ssl, install_nix |
| postgres_manage | many: files.Template, runr dispatch, command.New, apt.Install, **iptables** | `pg_runner`, `pg_sputnik`, `pg_plotter`, `pg_free_space_guard`, `earlyoom`, `syslog-ng`, `rpglotd`, `rpglot-web`, `patroni` removal, `pkill -SIGHUP -u postgres` defer | DSCP marking (iptables rules), prometheus exporters, extensions, disk_guard, perf_monitor |
| prepare | files, command.New | `systemd-logind` restart | infra_cgroup setup |
| repos | apt repo configuration, files.Template | (apt update implicit) | risk gating, repo file generation |
| sysctl | files.Create + `sysctl -p` | — (sysctl reload is a one-shot command) | `command.New().Execute("sysctl -p /etc/sysctl.d/60-chiit.conf")` |
| tunefs | OS-specific tunefs | — | platform-dispatched |

### Beyond apt/file/template/runr.*/systemd.*

The primitives that postgres-chiit actively uses beyond the bosun MVP set:

- **command.New().Execute** — anywhere bosun would want a `command.run` / `shell.exec` primitive. Active in `marking_DSCP.go`, `sysctl/main.go`, `postgres_manage/status.go`. Notably gated by `OnlyIF`/`NotIF` predicates (`systemctl is-active`-style guards) inside `chiit/lib/providers/command/types.go`.
- **iptables rule manipulation** — `DSCP` marking in `postgres_manage/marking_DSCP.go` uses raw `exec.Command("iptables", ...)`. Bosun candidate: `iptables.rule` or `netfilter.rule`.
- **cron file management** — `pg_mediator/cron.go` writes `/etc/cron.d/` files. Bosun candidate: `cron.entry`.
- **sysctl** — `sysctl/main.go` writes `/etc/sysctl.d/60-chiit.conf` then runs `sysctl -p`. Bosun candidate: `sysctl.param` or `kernel.parameter`.
- **users/groups** — chiit/lib/providers/users exists with group.go/user.go primitives. Bosun candidate: `user.account`, `group.entry`.
- **symlinks** — `files.Link` from `chiit/lib/providers/file/filemanager/link.go`. Bosun candidate: `file.symlink` (extension to `file.content`).
- **package install with version pin** — chiit's `apt.Install` accepts `&packages.Package{Name, Version}`. bosun-primitives `apt_package` already does this; check parity.

The first wave for parity is: `command.run` (with predicate guards), `file.symlink`, `sysctl.param`, `user.account`/`group.entry`. iptables and cron are second-wave.

## 6. Architectural problems to solve

### 6.1 Notification storm

100 file changes notifying `nginx` → 1 restart. **Solved trivially by chiit's filename dedup.** Spec contract: a deferred action keyed by `(action_type, target_id)` is idempotent; second insert is a no-op.

### 6.2 Validation before restart

`pooler/install_pg_doorman.go:330` already demonstrates the pattern: `files.TemplateWithValidation(ctx, dest, tmpl, vars, "/usr/bin/pg_doorman -t /etc/pg_doorman/pg_doorman.toml.new")`. Algorithm (`lib/files/file.go:84-156`):

1. Copy current config to `<path>.new`.
2. Render template to `<path>.new`.
3. If identical to existing → no validation needed, no change.
4. Else run validator command on `<path>.new`. **If it fails: panic** — file `<path>.new` stays for forensics, original is untouched, no notify fires.
5. `os.Rename(<path>.new, <path>)` — atomic swap. Notify fires.

Lessons:
- Validation MUST run on the new file, not the live one.
- Validator failure MUST NOT leave the config partially updated.
- Validator failure MUST NOT enqueue a restart/reload (the `.Notify(...)` callback never runs because the panic propagates first).
- The pattern is two-phase: render → validate → swap. Bosun should expose this as a first-class `validate_with =` parameter on the file primitive.

### 6.3 Health check after restart

Currently chiit has no concept. The closest analog is `runr/manage.go:Start` which inspects `ListServiceStatuses()` for `state == "Running"` before deciding `Changed`. For systemd via dbus the verification primitive is `InvocationID` change + post-restart `ActiveState=="active"` poll (see [Jon Writes Code on waiting for systemd jobs](https://jonathangold.ca/blog/waiting-for-systemd-job-to-complete/)).

For bosun, the proposal: `service.health_check_url = "http://127.0.0.1/healthz"` or `service.health_check_cmd = ["curl", "-fsS", "http://127.0.0.1/healthz"]`, with a `wait_for` budget. On health-check failure: the restart is recorded as failed in the journal but **the journal entry is removed** (the restart did run; the unit is just unhealthy). Apply report surfaces it as `Outcome::Failed`. Alternative: keep journal entry, retry next run — this is the chiit-style "at-least-once".

### 6.4 Ordering

chiit defers run in sorted filename order via `sort.Strings(list)` (`defers/process.go:33`). This is roughly: reloads before restarts for the same target (`reload-*` < `restart-*` alphabetically), and services in alphabetical order. **Stable but not principled.**

The principled order: deferred actions run **after** all resources have applied. Per-target dedup means at most one action per target per converge. For dependencies between deferred actions (e.g., restart pg_doorman *after* restart pg_doorman_users), the journal should carry a partial-order key — but in practice for postgres-chiit, alphabetical worked.

### 6.5 Cross-role notifies

Today chiit roles directly reach into other roles' service names: `roles/postgres/pg_probackup.go:93` does `defers.AddReloadRunr(ctx, "ssh")`. No load-time dependency on a "ssh" role. This works because notify-targets are just strings, not handles.

For bosun this becomes a question about the Starlark API. If notify targets are typed `Handle`s, then a cross-bundle notify requires both bundles to be loaded and the target handle to be re-exportable. Two options:
- **String identity (chiit's approach)**: `notify=["systemd.service:ssh"]`. Loose coupling, easy to break with typos.
- **Handle identity (bosun's current approach for `reload_on=[...]`)**: requires the handle to be in scope. Tighter, but cross-bundle is awkward.

Pragmatic answer: both. Handles for in-bundle; opaque strings (`service.handle_by_name("ssh")`) as an escape hatch.

### 6.6 Restart vs reload vs reload-or-restart

systemd offers `ReloadOrRestartUnit` — reload if `ExecReload=` is set, restart otherwise. This is what callers want 95% of the time. The chiit code distinguishes restart and reload explicitly with the rule "restart subsumes reload" (so if you've already enqueued a restart, an incoming reload is dropped). Reverse — "reload upgraded to restart by a later notify" — is what `AddRestart*` does explicitly via `RemoveReload*`.

For bosun: expose `service.restart`, `service.reload`, `service.reload_or_restart` as distinct deferred-action kinds. Dedup with priority: `restart > reload_or_restart > reload`. A later, higher-priority notify replaces (and removes) the lower.

### 6.7 Init-system abstraction

Three plausible API shapes:

1. **Concrete primitives only**: `runr.service` and `systemd.service` are separate. Author writes both with an `if fact == ...` switch. Mirrors chiit's `IsRunRInit()` checks.
2. **Abstract primitive**: `service.unit` dispatches at apply-time based on `inv.facts.init_system`. Author writes one spec.
3. **Both**: `service.unit` is the abstract, but `runr.service` / `systemd.service` are reachable for power users who need init-specific knobs (e.g., runr's `[Log]` block or systemd's `ConditionPathExists=`).

**Recommendation: option 3.** The `service.unit` Starlark name calls into one of two primitives behind the scenes; the underlying primitive set stays explicit and the abstract is a thin wrapper. Power users opt into `runr.*` or `systemd.*` directly.

## 7. Proposed defer journal design

### Layout

- Directory: `/var/lib/bosun/defers/` (NOT `/tmp` — chiit's bug).
- Mode 0700, owned by root.
- One file per pending deferred action.

### Format

Hybrid: chiit's "the file is the command" simplicity, plus a JSON header for structure. Each file is named `<sortkey>-<dedup-id>.deferred` and contains:

```json
{
  "spec_version": 1,
  "id": "systemd.restart:nginx",
  "action": "restart",
  "init_system": "systemd",
  "target": "nginx.service",
  "validate_cmd": null,
  "health_check": null,
  "priority": "restart",
  "enqueued_at": "2026-05-19T14:32:11Z",
  "enqueued_by": ["file.content:/etc/nginx/nginx.conf", "file.content:/etc/nginx/sites-enabled/default"]
}
```

The `id` is the dedup key. `<sortkey>` is two ASCII chars: priority-band (`r0` = restart, `r1` = reload_or_restart, `r2` = reload) then numeric phase. Files sort lexicographically into the desired execution order: phase 1 daemon-reload, phase 2 reload+restart by target.

Why JSON not shell? Three reasons:
- bosun is Rust; constructing safe shell is harder than constructing safe JSON.
- A future `bosun status` should be able to enumerate pending defers without an interpreter.
- `enqueued_by` makes operator debugging trivial — "why is nginx scheduled to restart? — because file.content:/etc/nginx/nginx.conf changed".

### Atomicity

Same as chiit:
1. Write `<dir>/<sortkey>-<id>.deferred.tmp.<nanos>`.
2. `fsync(file)`.
3. `rename(tmp, final)`.
4. `fsync(dir)`. **chiit skips this — bosun should not.**

This sequence is the standard pattern from the SQLite WAL docs ([Write-Ahead Logging](https://sqlite.org/wal.html)) and is the crash-consistency floor.

### Idempotent execution

Each deferred action is executed exactly once per success — but the execution itself may run zero, one, or many times (chiit-style at-least-once). Therefore the action must be idempotent:

- `restart` is idempotent if "restart twice in 5 seconds" is acceptable for the service. For ~all services, yes.
- `reload` is idempotent (`SIGHUP` twice does no harm).
- Arbitrary commands (`pkill -HUP`) — the operator's responsibility to keep idempotent.

### Replay

`bosun apply` start:
1. Open `/var/lib/bosun/defers/`. If absent, skip.
2. List files, sort lexicographically.
3. For each file:
   - Read JSON header.
   - Execute action (dbus/HTTP call for runr).
   - If success → delete file (`unlink` + `fsync(dir)`).
   - If failure → log + keep file + continue to next.
4. Run the apply phase.
5. Replay again (catches anything enqueued during apply).

The double replay is the chiit pattern (`cmd/run.go:61` and `cmd/run.go:90`). Pre-apply ensures we converge before doing more work; post-apply fires the notifies enqueued by the current run.

### Pruning

- If a target service is deleted (the role removed), the deferred restart is still attempted. systemd returns NoSuchUnit → bosun logs warning, removes the file.
- A defer older than 7 days that fails repeatedly → log loudly, keep file. Bosun never silently abandons work; operator decides.

### Observability

- `bosun status` reads the directory and prints pending defers.
- Each apply emits a tracing span per deferred action with `action_id`, `target`, `result`.
- Prometheus metrics: `bosun_defers_pending`, `bosun_defers_executed_total{result}`, `bosun_defers_replay_total`.

## 8. Proposed primitives + Starlark API sketches

### `runr.service` / `runr.timer` / `runr.cgroup`

Already drafted in the runr concept. Spec accepts the `Service`/`Timer`/`Cgroup` schema from `runr/types.go`. Apply writes `/etc/runr/<name>.service` (file primitive under the hood), then HTTP POST `daemon-reload` + `start` if not running.

### `systemd.service` / `systemd.timer`

Spec mirrors a systemd unit file. Apply writes `/etc/systemd/system/<name>.service`, calls `Manager.Reload` (daemon-reload) over dbus if `NeedDaemonReload` property is true, then `EnableUnitFiles` + `StartUnit`. Verification: subscribe to `JobRemoved` for the returned job path, then sample `ActiveState`.

### Abstract `service.unit`

Starlark sketch:

```python
service.unit(
    name = "nginx",
    type = "simple",
    exec_start = "/usr/sbin/nginx -g 'daemon off;'",
    restart = "on-failure",
    user = "www-data",
    autostart = True,
    # validation + health
    validate_with = ["nginx", "-t"],
    health_check = "http://127.0.0.1/health",
    health_check_timeout = "30s",
)
```

Dispatch: read `inv.facts.init_system` ∈ {`runr`, `systemd`}; route to one of the concrete primitives. The fact comes from bosun-facts (which is the right place — `init_system` is a host fact).

### `file.content` + `file.template` notify

Starlark sketch:

```python
nginx_conf = file.template(
    path = "/etc/nginx/nginx.conf",
    source = "templates/nginx.conf.tmpl",
    vars = vars,
    validate_with = ["nginx", "-t", "-c", "{path}"],  # path-substituted on new file
    reload_on = [],
    restart_on = [],
)

nginx_svc = service.unit(
    name = "nginx",
    ...
    reload_on = [nginx_conf],   # nginx_conf is a Handle
)
```

When the file changes and validates, the `service.unit` apply doesn't actually restart synchronously — it inspects `reload_on` and enqueues a defer to `/var/lib/bosun/defers/`. The defer runs in the replay phase at the end of the apply.

### `command.run` (for raw escapes)

Starlark sketch:

```python
command.run(
    name = "hup-pg-doorman",
    cmd = ["pkill", "-HUP", "pg_doorman"],
    deferred = True,
    only_if = ["pgrep", "pg_doorman"],
)
```

`deferred=True` skips immediate execution and enqueues a `command.run` action into the journal. This is the analog of chiit's `defers.AddCommand`.

## 9. Open questions

1. **Failure semantics on chained defers.** If defer-A fails, should defer-B still run? Ansible's `force_handlers` default is "stop on first failure"; chiit logs+continues. Both are defensible. **Lean: continue, but mark apply as failed.**
2. **Where does the validator run?** On `<path>.new`, but: should it run inside the apply step (synchronous, blocks the converge) or as part of the defer replay (synchronous to the replay)? In chiit, validation is *synchronous to the file render* (block the file primitive until valid). This is right — bosun should do the same. The defer entry is only enqueued *after* validation passes.
3. **Health-check failure: retry-forever or fail-once?** chiit retries forever (defer file stays). Ansible doesn't retry. For services like `nginx` where a bad config = down service, retry-forever is dangerous (now you have a permanent retry loop). **Lean: retry up to N times, then promote to a failed-state file that requires operator intervention to clear.**
4. **What is the cross-bundle notify story?** Probably `service.by_name("nginx")` returning a `Handle` whose target is resolved at apply-time, not load-time. This is the Salt-style "string identity" escape hatch.
5. **Where does `service.health_check` actually run?** From bosun-cli (the agent) on the same host as the unit. Fine on bare metal. Inside a pod where bosun runs alongside runr-init, also fine. Across hosts, never — health checks are local.
6. **Runr API gaps**: there's no JobRemoved-equivalent. To detect "did the restart succeed", we have to poll `/api/v1/services/statuses` and compare `Restarts` or `StartedAt`. Should bosun ask runr maintainers for a synchronous-wait endpoint, or live with polling?
7. **Pre-systemd `daemon-reload` cost**: chiit calls `daemon-reload` before every state change (`manager_dbus.go:164`). On hosts with thousands of units, daemon-reload is slow. Bosun should respect `Manager.NeedDaemonReload` property (chiit already does — `needDaemonReload` in `manager_dbus.go:85`) and call it once per converge, not once per unit.
8. **Validator command argument substitution**: chiit hardcodes `pg_doorman -t /etc/pg_doorman/pg_doorman.toml.new` (`.new` suffix is a chiit convention). bosun should template it: `validate_with = ["nginx", "-t", "-c", "{new_path}"]`.

## 10. Concrete next steps

1. **Decide the journal record format** (JSON-headed file vs single JSON-lines vs SQLite). Recommendation above is JSON-per-file because it matches chiit's debuggability and avoids a transactional dependency. Sign off in the spec.
2. **Decide the abstract vs concrete primitive split.** Option 3 (`service.unit` wrapping `runr.*`/`systemd.*`) is the proposal. Confirm or pick another.
3. **Audit polkit assumptions.** Decide whether bosun is allowed to assume root, or whether non-root with polkit rules is a supported deployment. Affects dbus error handling.
4. **Lock in the runr-vs-systemd fact source.** `inv.facts.init_system` is the obvious answer; the fact gatherer reads `/proc/1/comm` and `/etc/runr/` existence. Spec this as `bosun-facts::InitSystem`.
5. **Spike: zbus_systemd integration in bosun-primitives.** Just enough to `Manager.RestartUnit` with `JobRemoved` waiting and `InvocationID` verification. 2-3 days of work; resolves whether the crate is fit for purpose before committing the primitive design.

## File index

- `/home/vadv/Projects/bosun/postgres-chiit/lib/defers/process.go` — replay loop.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/defers/custom.go` — atomic write.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/defers/systemd.go` — systemd defers (restart/reload, mutual exclusion).
- `/home/vadv/Projects/bosun/postgres-chiit/lib/defers/runr.go` — runr defers.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/defers/defers_test.go` — dedup tests.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/runr/client.go` — runr HTTP API.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/runr/types.go` — runr config schema.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/runr/chiit.go` — runr install + runr-init dispatch.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/runr/manage.go` — idempotent start/stop/restart.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/runr/systemd.go` — systemd→runr config conversion.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/systemd/manager_dbus.go` — go-systemd dbus client.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/systemd/manager.go` — interface + shell/dbus switch.
- `/home/vadv/Projects/bosun/postgres-chiit/lib/files/file.go` — `TemplateWithValidation`.
- `/home/vadv/Projects/bosun/postgres-chiit/cmd/run.go:61, :90` — defers replay points.
- `/home/vadv/Projects/bosun/chiit/lib/handlers/types.go` — `Notify`/`NotifyChain`.
- `/home/vadv/Projects/bosun/bosun-client/crates/bosun-core/src/resource.rs:105` — current `reload_on`/`depends_on` model.
- `/home/vadv/Projects/bosun/bosun-client/crates/bosun-core/src/orchestrator.rs:407` — current `Outcome::Deferred` semantics (not the same as journaled deferred restart; it's "primitive said this is retryable").
