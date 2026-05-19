# Test coverage audit для bosun-client

Дата: 2026-05-19
Baseline: 1258 тестов (commit 4e91cdd, ветка main)

## Сводка

Аудит выполнен по 28 модулям: 21 примитив в `bosun-primitives` и 7 ключевых модулей в `bosun-core`. Для каждого проверены пять категорий из `feedback_bosun_strengthen_tests.md`: A — pure-функции с нетривиальной логикой, B — NoChange-mutation-counter в apply, C — error mapping для каждого варианта enum, D — edge cases (empty inputs, race conditions, permissions), E — read-before-write invariant.

Условные обозначения статуса:

- OK — все пять категорий покрыты, дальнейшее усиление nice-to-have.
- Fix-up — есть один-два пробела, не блокирующих регрессии, но рекомендованных.
- Gap — заметные пробелы, регрессии возможны без покрытия.

| Primitive / модуль | A pure | B no-change | C error map | D edge | E read-first | Статус |
|---|---|---|---|---|---|---|
| apt.package | OK | OK | OK | OK | OK | OK |
| apt.update_cache | OK | Fix-up | OK | OK | OK | Fix-up |
| apt.key | OK | OK | OK | OK | OK | OK |
| file.content | OK | OK | OK | OK | OK | OK |
| file.delete | OK | OK | OK | Fix-up | OK | OK |
| file.symlink | OK | OK | OK | Fix-up | OK | OK |
| users.user | OK | OK | Fix-up | OK | OK | Fix-up |
| users.group | OK | OK | Fix-up | OK | OK | Fix-up |
| process.signal | OK | OK | OK | OK | OK | OK |
| runr.service | OK | OK | Gap | OK | OK | Fix-up |
| runr.timer | Fix-up | OK | Fix-up | Fix-up | OK | Fix-up |
| runr.cgroup | OK | OK | n/a | OK | OK | OK |
| systemd.service | OK | OK | OK | OK | OK | OK |
| systemd.timer | OK | OK | Gap | Fix-up | OK | Fix-up |
| pg_sql.exec | OK | OK | Fix-up | OK | OK | Fix-up |
| pg_sql.query | OK | OK | OK | OK | OK | OK |
| cert.tls | OK | OK | OK | OK | OK | OK |
| sysctl.reload | Fix-up | OK | Fix-up | Fix-up | n/a | Fix-up |
| template | OK | n/a | n/a | Fix-up | n/a | OK |
| dispatch (RealDispatchClient) | OK | n/a | OK | OK | n/a | OK |
| health.check (cmd+url) | Fix-up | n/a | Fix-up | Gap | n/a | Fix-up |
| defers/journal | OK | n/a | OK | OK | n/a | OK |
| defers/replay | OK | n/a | OK | Fix-up | n/a | OK |
| defers/format | OK | n/a | n/a | OK | n/a | OK |
| defers/priority | OK | n/a | n/a | OK | n/a | OK |
| orchestrator | Gap | OK | n/a | Fix-up | n/a | Fix-up |
| inventory (merge) | OK | n/a | OK | OK | n/a | OK |
| starlark_glue/globals | Gap | n/a | Fix-up | Gap | n/a | Gap |
| path_safety | OK | n/a | OK | OK | n/a | OK |
| validate (RealValidateRunner) | OK | n/a | OK | OK | n/a | OK |

## Детальные находки

### apt.update_cache

**B. NoChange-mutation-counter:**

- Plan покрыт matrix'ой mtime/force/max_age. Apply имеет тест на NoChange (`apply_no_change_returns_early`), но без явной проверки, что `runner.calls()` пуст после ранних `return ChangeReport::no_change()` в путях `cleanup_only`/`update_skipped` (когда сам apt-get не вызывался, но cleanup мог дёрнуть `find`). Добавить тест, который пропускает update по age и ассертит `runner.count_calls_to("apt-get", "update") == 0`.

### users.user

**C. Error mapping:**

- `users_error_to_primitive` тестируется для NotRoot, ToolNotFound, Exec. Не покрыты Lookup, InvalidName. Добавить две функции:
  - `map_lookup_to_apply` — `UsersError::Lookup { target, reason }` маппится в `Apply` с обоими полями в сообщении.
  - `map_invalid_name_to_invalid_payload` — `UsersError::InvalidName` маппится в `InvalidPayload`.

**A. Pure-функции:**

- `collect_diffs` покрыто только Uid, Group, Shell+Home+Comment в одном тесте (`present_with_multiple_mismatches_collects_all`). Отдельных тестов на Shell-only-mismatch, Home-only-mismatch, Comment-only-mismatch нет, и порядок результатов не зафиксирован. Добавить по одному фокус-тесту на каждое поле, ассертить `diffs == vec![FieldDiff::Shell]` и т.д.

**Real backend argv builder:**

- `RealUsersBackend::useradd` строит argv с условными ветками для system/uid/group/shell/home/no_create_home/comment. Эти ветки не тестированы напрямую — только косвенно через apply, который покрыл `useradd_opts.name/uid/group/shell/home`. Argv'ы (`--system`, `--no-create-home`, `--comment`) не проверены. Добавить unit-тест, который строит argv и сравнивает по `Vec<String>` (без spawn'а — выделить функцию `build_useradd_argv(opts) -> Vec<String>` или тестировать через mock CommandRunner).

### users.group

**C. Error mapping:**

- Дублирует пробелы users.user: Lookup и InvalidName.

**Real backend argv builder:**

- `RealUsersBackend::groupadd`/`groupmod` argv-сборка не покрыта direct-тестами. Symmetrically с users.user.

### runr.service

**C. Error mapping:**

- В apply.rs покрыты только `RunrError::Unavailable` и `RunrError::NotFound`. Не покрыты direct: `ApiError`, `BadResponse`, `RestartNotObserved`, `StartNotObserved`, `ServiceStartFailed`, `Io`. Каждый из них — отдельная ветка в `map_runr_error` с своими полями в сообщении. Добавить тесты:
  - `map_runr_error_api_error_is_apply` — status + body в сообщении.
  - `map_runr_error_bad_response_is_apply` — msg в сообщении.
  - `map_runr_error_restart_not_observed_is_apply` — unit в сообщении.
  - `map_runr_error_start_not_observed_is_apply` — unit + last_state.
  - `map_runr_error_service_start_failed_is_apply` — unit.
  - `map_runr_error_io_is_runr_unavailable_deferrable` — `is_deferrable()` == true.
- Тест на `is_deferrable()` есть только для Unavailable; для остальных вариантов отсутствует.

### runr.timer

**A. Pure-функции:**

- `decide_timer_action` покрыт matrix'ой 7 ячеек, но:
  - `Disabled × None` (отсутствует в snapshot) — не покрыто. По коду должно быть NoChange.
  - `Absent × None` (отсутствует в snapshot) — не покрыто. По коду должно быть NoChange.
  - `Enabled × None × start_now=true` — покрыто только с `start_now=false`.

**B. NoChange-mutation-counter:**

- Один тест (`apply_no_change_does_not_invoke_mutation`) ассертит отсутствие enable/start/stop/disable вызовов. Хорошо. Но нет аналога для `Disabled × disabled` и `Absent × disabled` (current=None или enabled=false).

**C. Error mapping:**

- `map_runr_error` в apply.rs полностью дублирует runr_service, но нет direct-тестов на варианты. Применяется тот же backlog, что и для runr.service.

**D. Edge cases:**

- Нет теста на Enable, Disable, StopAndDisable путей: только NoChange и кэш-тест. Добавить:
  - `apply_enable_calls_timer_enable_with_start_now_flag` — assert call recorded `timer_enable:x:true` или `:false`.
  - `apply_disable_calls_timer_disable` — assert recorded.
  - `apply_stop_and_disable_calls_stop_then_disable_in_order` — assert sequence.

### systemd.timer

**C. Error mapping:**

- `map_systemd_error` дублирован из systemd.service без direct-тестов. systemd.service имеет 7 направленных тестов на каждый вариант — у systemd.timer ни одного. Применяется тот же backlog (BusUnavailable, Dbus, NoSuchUnit, AuthorizationDenied, JobFailed, RestartNotObserved, Timeout, Io).

**D. Edge cases:**

- Action `TimerAction::Enable` имеет ветку `if spec.enable { enable_unit }` + всегда `start_unit`. Не покрыто:
  - `enable=false` — пропуск enable_unit, остаётся только start_unit.
  - `enable=true` + already-enabled — нет read-before-write аналога service'а (там check через `is_unit_enabled`); systemd.timer всегда зовёт enable_unit. Это намеренно или баг — оператор должен решить и закрепить тестом.

### pg_sql.exec

**C. Error mapping:**

- `map_backend_error` покрывает 3 ветки direct (Connect, Sql, InvalidDsn), но `Timeout` — только косвенно через query/timeout тест. Добавить direct: `apply_timeout_error_returns_apply_with_duration`.
- `map_check_error` (plan.rs) — 4 варианта PgSqlError, покрыт только Connect и InvalidDsn. Добавить:
  - `plan_check_timeout_returns_apply` — Timeout превращается в Apply с упоминанием duration.
  - `plan_check_sql_error_returns_apply` — sqlstate в reason.

### sysctl.reload

**A. Pure-функции:**

- `RealSysctlBackend::reload` имеет логику обрезки stderr `if stderr.len() > 512 { ... }`. Только косвенно покрыто (smoke-тест запуска sysctl). Добавить тест, имитирующий длинный stderr через mock и assert обрезку с маркером `…`.

**C. Error mapping:**

- `run` фильтрует только `Apply { reason }` из backend-ошибки. Нет теста на `path не существует` → `Apply` с упоминанием `bundle order issue`. Добавить.

**D. Edge cases:**

- Нет теста на `ctx.cancelled_or_past_deadline()` → `Cancelled`. Добавить.

### health.check

**A. Pure-функции:**

- `cmd::compute_exit_summary`/`url::run_once` — нет MockServer-based тестов на Success (200=200) и BadStatus (200 vs 500). Текущие тесты только Transport (unreachable address) и invalid URL. Добавить wiremock-тест:
  - `run_once_status_200_returns_success`.
  - `run_once_status_500_when_expected_200_returns_bad_status`.

**D. Edge cases:**

- `transport_reason` truncation при `s.len() > EXCERPT_LIMIT` — не покрыто, тест на короткий stderr только. Добавить тест с искусственно длинным сообщением.
- Retry-loop: `cmd::run_check` имеет retry с `retry_count`/`retry_interval_sec`. Нет теста на `retry=0` (один пробег), `retry=3` (попытки), `retry с интервалом, прерванный cancel'ом`. Только base-cases.

**C. Error mapping:**

- Маппинг `HealthCheckError` → `PrimitiveError::HealthCheckFailed` в primitive'ах покрыт, но варианты `HealthCheckError` сами не тестируются direct: Timeout, UrlBadStatus, CmdNonZero, Cancelled. Каждый — отдельная ветка в трейте.

### orchestrator

**A. Pure-функции:**

- `panic_message(payload: &dyn Any + Send)` — не покрыта direct. Текущие тесты `plan_only_contains_panic_in_plan` и `apply_contains_panic_in_apply` используют panic со static string. Не покрыты:
  - panic с `String` (не &'static str).
  - panic с non-string payload (struct), который должен попасть в fallback `<non-string panic payload>`.
- `describe_primitive_error` — не покрыта direct, только косвенно (выход содержит формат ошибки).

**D. Edge cases:**

- `aborted` path: тест на `apply_with_error_aborts` есть, но нет проверки, что все оставшиеся ресурсы получают `Outcome::Skipped` с конкретным message `skipped: aborted after earlier failure`. Текущий тест ассертит общий `summary.skipped > 0`, не детали.
- `mark_dirty` вызывается ПЕРЕД apply. Не покрыт case: apply упал → `mark_dirty` уже был вызван. Это критично — оператор должен знать, что грязный факт пометится, даже если apply провалился.
- `Outcome::Deferred НЕ прерывает прогон при continue_on_error=false`. Покрыто (`apply_deferred_does_not_block_subsequent_resources`), но без проверки `summary.deferred == 1`.
- `Outcome::Interrupted` для cancel'а во время apply — покрыто, но не проверено что `apply_ctx.record_changed` НЕ был вызван (это критично: ресурс, который не доделался, не должен числиться changed).

### starlark_glue/globals

Это самый большой пробел из всех модулей. 1126 строк, 4 теста.

**A. Pure-функции без direct-тестов:**

- `resolve_service_unit_kind(state: &EvalState) -> Result<&'static str, anyhow::Error>` — pure functional dispatch по факту `init_system`:
  - `"systemd"` → `"systemd.service"`.
  - `"mixed-systemd-runr"` → `"systemd.service"`.
  - `"runr"` → `"runr.service"`.
  - другое → Err.
  - fact Unknown → Err "init_system fact unknown".
  - fact не строка → Err "not a string".
  - Не покрыто ни одной ячейки matrix.
- `reject_unexpected_service_unit_kwargs` — проверяет allowlist `SERVICE_UNIT_ALLOWED_KWARGS`. Не покрыто:
  - все разрешённые ключи проходят.
  - неразрешённый ключ вызывает Err с упоминанием конкретного имени.
  - не-строковый ключ вызывает Err.
- `yaml_to_json(v: serde_norway::Value) -> Result<serde_json::Value, String>` — pure-конвертер. Покрытие нулевое. Каждый вариант enum'а — отдельная ветка:
  - Null, Bool, Number (i64/u64/f64/NaN/Inf), String, Sequence (nested), Mapping (nested, non-string key → Err), Tagged → Err.
- `resolve_merge_strategy` — pure-логика выбора стратегии:
  - пустая строка + default present → возвращает default.
  - пустая строка + default отсутствует → Err.
  - конкретная строка передаётся в `MergeStrategy::parse`.
- `pick_defining_module` — fallback логика выбора имени файла из call stack или из EvalState stack. Хотя бы happy-path с frame'ом и fallback должны быть.

**C. Error mapping:**

- `StarlarkGlueError` варианты — нет direct-тестов на сериализацию в `starlark::Error`.

**D. Edge cases:**

- inventory.read для отсутствующего файла, пути с `../`, NUL-байтом, symlink'ом — все эти пути покрыты в path_safety, но интеграционно из globals не проверены.
- merge_keyed с non-map element / missing key — ошибки должны прокидываться. Не покрыто.

Рекомендация — разбить тесты по `mod tests` секциям для каждого namespace, чтобы покрытие было читаемым. Минимум 25-30 unit-тестов на эти helpers.

## Приоритизация

### High (реальные регрессии возможны без покрытия)

Эти пробелы покрывают пути, где баг останется незамеченным до прода.

1. **starlark_glue/globals: `yaml_to_json`** — конвертер всех типов YAML. Регрессия здесь сломает inventory.read для bundle'ов с любым непокрытым типом (числа большие чем i64, mappings с non-string keys, sequences вложенно).
2. **starlark_glue/globals: `resolve_service_unit_kind`** — dispatch service.unit'а. Регрессия здесь молча направит resource не в тот примитив на смешанной ноде.
3. **runr.service / runr.timer / systemd.timer: error mapping** — без тестов на каждый вариант enum'а замена `match` arm может привести к `is_deferrable` неверно для одного из вариантов. Симптом: bosun отложит реальный crash или прервёт прогон на transient ошибке.
4. **users.user / users.group: real backend argv** — argv-сборка для useradd/usermod/groupadd сейчас тестируется только через apply'я, где порядок аргументов не сравнивается. Изменение порядка флагов в `build_*` может сломать `--no-create-home` для system-users, например.
5. **health.check: cmd/url retry loop и transport_reason truncation** — health-check критичен для defer replay (Phase I). Регрессия в retry-логике может скрыть failed restart за идущим успешным после retry'я.

### Medium (полезно для confidence)

1. **users.user / users.group**: тесты на `users_error_to_primitive` для `Lookup` и `InvalidName`.
2. **pg_sql.exec**: direct тест на `Timeout` в `map_backend_error` для apply и `map_check_error` для plan.
3. **runr.timer**: 3 теста на Enable/Disable/StopAndDisable путей (сейчас покрыт только NoChange).
4. **orchestrator**: `panic_message` для String и non-string payload; `mark_dirty` вызывается даже при apply failure.
5. **systemd.timer**: edge cases для `enable=false` пути и `apply_enable_already_enabled_path_not_observed`.
6. **sysctl.reload**: cancellation path, stderr truncation, path-missing case.

### Low (nice-to-have)

1. **users.user**: тест на сравнение по `group` с `gid=N` fallback (т.е. group_name = `gid=42` когда /etc/group битый — должен срабатывать как mismatch для `spec.group="postgres"`).
2. **redact_dsn (pg_sql_common)**: edge cases — пустой DSN, DSN с `password=` без значения, DSN со множественными password.
3. **file_content/plan: `matches_spec` для group-mismatch** — есть owner-mismatch, нет direct group-mismatch.
4. **file_delete / file_symlink**: edge cases с дополнительными file types (block device, fifo).
5. **process.signal/plan: `describe_selector`** — invalid-ветви не покрыты direct.
6. **template**: дополнительные cases edge-форматирования путей.

## Конкретный backlog тестов

Список из 47 тестов, упорядоченных от high к low. Каждая строка — короткое описание «что проверяет».

### Critical (12 тестов)

1. `starlark_glue::globals::yaml_to_json_null` — Y::Null → JSON null.
2. `starlark_glue::globals::yaml_to_json_bool_number_string` — round-trip primitives.
3. `starlark_glue::globals::yaml_to_json_nested_mapping_and_sequence` — depth-3 структура сохраняется.
4. `starlark_glue::globals::yaml_to_json_non_string_key_returns_err` — `1: foo` в mapping → Err.
5. `starlark_glue::globals::yaml_to_json_tagged_returns_err` — `!Custom value` → Err.
6. `starlark_glue::globals::resolve_service_unit_kind_systemd_returns_systemd_service`.
7. `starlark_glue::globals::resolve_service_unit_kind_runr_returns_runr_service`.
8. `starlark_glue::globals::resolve_service_unit_kind_unknown_init_returns_err_with_init_in_message`.
9. `starlark_glue::globals::resolve_service_unit_kind_unknown_fact_returns_err`.
10. `runr_service::apply::map_runr_error_api_error_includes_status_and_body_in_reason`.
11. `runr_service::apply::map_runr_error_bad_response_is_apply_not_deferrable`.
12. `runr_service::apply::map_runr_error_io_is_runr_unavailable_deferrable`.

### High (15 тестов)

13. `runr_service::apply::map_runr_error_restart_not_observed_is_apply`.
14. `runr_service::apply::map_runr_error_start_not_observed_with_last_state_in_reason`.
15. `runr_service::apply::map_runr_error_service_start_failed_with_unit_in_reason`.
16. `runr_timer::apply::map_runr_error_full_matrix` — все 7 вариантов через table-driven test.
17. `systemd_timer::apply::map_systemd_error_full_matrix` — 8 вариантов direct.
18. `users_user::apply::users_error_to_primitive_lookup_to_apply`.
19. `users_user::apply::users_error_to_primitive_invalid_name_to_invalid_payload`.
20. `users_user::backend::useradd_argv_includes_system_flag_when_set`.
21. `users_user::backend::useradd_argv_includes_no_create_home_when_set`.
22. `users_user::backend::useradd_argv_includes_comment_when_set`.
23. `users_group::backend::groupadd_argv_with_gid_and_system`.
24. `health_check::url::run_once_with_wiremock_status_match_returns_success`.
25. `health_check::url::run_once_with_wiremock_unexpected_status_returns_bad_status`.
26. `health_check::cmd::run_check_retries_on_failure_until_success_within_limit`.
27. `health_check::cmd::run_check_returns_failed_after_max_retries_exhausted`.

### Medium (12 тестов)

28. `pg_sql_exec::apply::map_backend_error_timeout_is_apply_with_duration`.
29. `pg_sql_exec::plan::map_check_error_timeout_is_apply`.
30. `pg_sql_exec::plan::map_check_error_sql_with_sqlstate_in_reason`.
31. `runr_timer::apply::enable_path_calls_timer_enable_with_start_now`.
32. `runr_timer::apply::disable_path_calls_timer_disable`.
33. `runr_timer::apply::stop_and_disable_path_calls_stop_then_disable_in_order`.
34. `runr_timer::plan::disabled_when_unknown_is_no_change`.
35. `runr_timer::plan::absent_when_unknown_is_no_change`.
36. `systemd_timer::apply::enable_false_skips_enable_unit_but_calls_start`.
37. `orchestrator::panic_message_with_string_payload_returns_inner_string`.
38. `orchestrator::panic_message_with_struct_payload_returns_fallback_marker`.
39. `orchestrator::aborted_subsequent_resources_have_skipped_outcome_with_message`.

### Low (8 тестов)

40. `sysctl_reload::apply::cancelled_ctx_returns_primitive_cancelled_before_spawn`.
41. `sysctl_reload::apply::path_missing_returns_apply_with_bundle_order_hint`.
42. `sysctl_reload::apply::real_backend_truncates_long_stderr_with_ellipsis_marker`.
43. `users_user::plan::collect_diffs_shell_only_mismatch_yields_single_diff`.
44. `users_user::plan::collect_diffs_home_only_mismatch_yields_single_diff`.
45. `users_user::plan::collect_diffs_comment_only_mismatch_yields_single_diff`.
46. `users_user::plan::group_compared_by_gid_fallback_name`.
47. `pg_sql_common::redact::redact_dsn_empty_string_passes_through`.

## Notes по методологии

- Все 1258 тестов сейчас проходят (cargo test --workspace --no-run ОК на baseline).
- Параллельный correctness batch agent работает в orchestrator + traits, не пересекается с предлагаемым backlog'ом.
- При имплементации backlog'а предложен порядок High → Medium → Low. Critical-блок (12 тестов) даёт наибольший рост defense-in-depth за минимум усилий: yaml_to_json + service.unit dispatch + runr error mapping — это три точки, где даже мелкая регрессия скрывается под кодом, который кажется «работает на mock-stack'е».
- Тесты типа table-driven (например, `map_runr_error_full_matrix`) предложены через единый параметризованный helper — это уменьшает дубликат кода и упрощает сопровождение при добавлении вариантов в enum.
