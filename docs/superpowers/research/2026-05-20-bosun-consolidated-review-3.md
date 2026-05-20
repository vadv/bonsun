# Bosun consolidated review 3
Date: 2026-05-20
Scope: bosun-client (без bosun-server scope)
Inputs: предыдущие ревью `/tmp/review-bosun.md`, `/tmp/review-bosun-2.md`, research 2026-05-19, memory feedback'ы, исходники, CI workflows.
Local verification: `cargo test --workspace` — 1418 passed; `cargo clippy --workspace --all-targets --locked -- --deny warnings` — clean.

## Executive summary

1. Блокеров уровня «отдай рут чужому» больше нет. Journal::open укреплён через `O_NOFOLLOW`+`fchmod` (B4 закрыт), Cancelled больше не deferrable (H1 закрыт), deadline-checker применяется и к loaded модулям (H2 закрыт), UnitName newtype есть (M3 закрыт), `--dry-run` больше не дёргает replay (blocker review-2 закрыт). Тестов 1418, BDD-сценариев 107, clippy clean.
2. Критичный регресс по локализации apt: код‑комментарий `apt_package/exec.rs:230` обещает «мы запускаем под `LC_ALL=C` через переменные среды CLI», но ни один `Command::env("LC_ALL"...)` не существует. На ноде с не‑английской локалью dpkg recovery (паттерн «dpkg was interrupted», «Unable to locate package») не сработает — recoverable превратится в hard failure. M2 review-1 НЕ закрыт, и теперь хуже: комментарий вводит в заблуждение.
3. BDD не запускается в CI. 107 сценариев — единственная страховка интеграции; никакой gate перед merge нет. `.github/workflows/ci.yml` гоняет только `fmt`/`clippy`/`cargo test --lib --bins --tests`. Любой PR может сломать BDD-сценарий, и зелёный CI это пропустит.
4. `bosun_runr_reachable` и `bosun_systemd_reachable` всё ещё означают «handle сконструирован», не «daemon reachable». HELP-текст обновлён до правды (`metric.rs:192,198`), но имя метрики противоречит семантике — оператор, увидевший `bosun_runr_reachable=1`, разумно ожидает что runr ответил на `daemon_info`. Дешёвый probe есть в trait'е (`RunrHandle::daemon_info()`), цена пробы — одна HTTP/dbus попытка.
5. `bundle validate` всё ещё штампует мок-примитивы: `NoopPrimitive::build_payload` (`bundle_validate.rs:233`) валидирует только `identity_keys`, а `template_fn` возвращает пустую строку. Любая Jinja2 syntax error и любая опечатка в payload-поле (`state="absnt"`, `mode=0x644` вместо `0o644`) до plan/apply не ловится. Pre-commit hook'и репозиториев бандла дадут оператору ложное «evaluate OK».
6. Парк через CI ходит за `dtolnay/rust-toolchain@stable` — никакого `rust-toolchain.toml`, никакого pin'а GitHub-action SHA, никакого pin'а docker base image в musl-build. Воспроизводимость на уровне «работает в этот понедельник».
7. `ApplyCtx` разросся до 15 полей через builder (`primitive.rs:329`), но `pub mod tracing_test_util` (`lib.rs:27`) и все 21 примитива выставлены `pub mod` без feature-gate. Внешний код может зацепиться за internal модули, и любая чистка крейт-боундари будет ломающей. L2 review-2 не закрыт.

## Findings

### BLOCKER (critical production risks)

Блокеров для production у самого бинаря нет — все критичные исправления из review-1/review-2 либо закрыты, либо имеют намеренное обоснование (defers в `/tmp` by design, runr/systemd как dependency без инструкции по решению user'а). Ниже — HIGH-уровень, не критично-блокирующий, но реальные регрессии возможны.

### HIGH (нужно до production)

#### H1. LC_ALL=C для apt/dpkg комментирован, но не выставлен в Command::env

- Файл: `bosun-client/crates/bosun-primitives/src/apt_package/exec.rs:230`.
- Симптом: на ноде с локалью `de_DE.UTF-8` / `ru_RU.UTF-8` stderr apt-get локализован. `analyze_install_result` ищет английские подстроки «dpkg was interrupted», «Unable to locate package» — паттерн-матч не сработает, recoverable error станет `OtherFailure` → hard fail вместо retry.
- Что хуже: код-комментарий обещает что среда выставлена «через переменные среды CLI». Это не так — `grep -rn "LC_ALL"` по `bosun-client/crates/` находит только этот комментарий.
- Решение: явный `Command::env("LC_ALL", "C").env("LANG", "C")` в `RealCommandRunner` для apt-цепочки и для всего `process_signal` / `users` / `sysctl`. Покрыть BDD: запустить apt примитив с переопределённой локалью, убедиться что dpkg-interrupted всё ещё распознаётся.

#### H2. BDD не запускается в CI

- Файлы: `.github/workflows/ci.yml`, `.github/workflows/musl-build.yml`, `.github/workflows/security.yml`.
- CI workflow гоняет только `cargo fmt --check`, `cargo clippy --deny warnings`, `cargo test --workspace --locked --lib --bins --tests`. Сценариев BDD 107 (включая `bundle_structure:17`, `users:9`, `service_unit_dispatch:7`, `file_content:7`), но их integration никак не gating PR.
- README:294 обещает `make test-bdd` — но это локальный target, не workflow job.
- Решение: добавить отдельный job `bdd` в `ci.yml` с `services: docker` или `runs-on: ubuntu-22.04` + setup-docker. Можно с `if: contains(github.event.pull_request.labels.*.name, 'run-bdd')` для PR-only on-demand, чтобы не платить за каждый push. Минимум — на main branch nightly.

#### H3. `bundle validate` мокает payload-валидацию и template-рендеринг

- Файл: `bosun-client/crates/bosun-cli/src/bundle_validate.rs:72-78` и `:233-247`.
- `template_fn` возвращает `Ok(String::new())` без рендеринга — Jinja2 syntax errors не ловятся.
- `NoopPrimitive::build_payload` валидирует только identity_keys и пропускает payload. Опечатка `state="absnt"` или неверный тип в kwargs проходит validate, но падает в plan/apply на ноде.
- Решение: переиспользовать продакшен‑`build_primitives` (без real backend'ов, но с реальными `build_payload`) и `RealTemplateFn` с ограниченным набором фактов из fixture. Жёсткий маркер «full render mode» через флаг — оператор знает, что fixture не покроет всё.

#### H4. `bosun_{runr,systemd}_reachable` означает «handle constructed», не «reachable»

- Файлы: `bosun-client/crates/bosun-cli/src/run.rs:238,247`; `metric.rs:191-200`.
- `runr_reachable = Some(true)` ставится сразу после `RunrClient::new(...)` — это конструктор без HTTP-вызова. Реальной достижимости daemon'а не проверяется.
- HELP-текст в metric.rs:192 обновлён до правды («1 if a runr handle was constructed this run»), но имя метрики (`reachable`) обещает другое. Operator‑side алерт «BosunRunrUnreachable: bosun_runr_reachable == 0» сработает только если CLI не построил handle — то есть на ноде с `init_system != runr`, не на сценарии «runr daemon упал».
- Решение: либо переименовать в `bosun_runr_handle_built` и `bosun_systemd_handle_built` (с HELP «handle was constructed; this is not a probe»), либо вызвать `RunrHandle::daemon_info()` один раз и пометить `reachable=true` только при успехе. Цена probe'а — один HTTP GET на localhost.

#### H5. Deadline не уважается в pacer-sleep и в orchestrator-цикле между ресурсами

- Файлы: `bosun-client/crates/bosun-core/src/orchestrator.rs:320`; `bosun-client/crates/bosun-core/src/health_check.rs:149`.
- `cancellable_sleep(pacer_interval, &apply_ctx.cancel)` проверяет только `cancel.is_cancelled()`, не сравнивает текущее время с `apply_ctx.deadline`. Если SIGTERM не пришёл, а `--deadline-sec` истёк — sleep досплится.
- Симптом: на парке 60k нод с pacer'ом `target=300s`, deadline=600s — если evaluate занял 250s, а потом 200 ресурсов через pacer наберут 30s‑intervals, оркестратор пройдёт мимо deadline'а и закончит «успешно» позже срока. SIGTERM от runr/systemd таймера должен прийти, но между ними окно.
- Решение: в `cancellable_sleep` добавить `deadline: Option<Instant>` или проверять `ctx.deadline` явно в orchestrator-цикле перед каждым ресурсом и до pacer-sleep'а. При past_deadline — `Outcome::Interrupted` для текущего и всех следующих ресурсов.

#### H6. M1 (review-1) не закрыт: file.content валидирует только leaf на is_symlink

- Файл: `bosun-client/crates/bosun-primitives/src/file_content/apply.rs:70-73`.
- `std::fs::symlink_metadata(target)` ловит только финальный компонент. Любой parent в цепочке (`/etc/foo/bar/...` где `/etc/foo` — symlink на `/var/private/foo`) пишет через symlink. Под root это значит, что local user, имеющий write на одну из родительских директорий, может перенаправить bosun писать в свою.
- Решение: либо обход parent chain через `symlink_metadata` пошагово, либо `openat(dirfd, O_NOFOLLOW)` (по аналогии с `Journal::open`). Acceptance: BDD-сценарий с подкладыванием symlink на parent.

#### H7. CI rust-toolchain не запинен

- Файлы: `.github/workflows/{ci,musl-build,security}.yml`; нет `bosun-client/rust-toolchain.toml`.
- Все workflow'ы используют `dtolnay/rust-toolchain@stable`. Когда выходит новый stable с поломкой/предупреждением — CI зеленый сегодня, красный в понедельник. Никакого pin'а на конкретную минорку.
- Решение: создать `bosun-client/rust-toolchain.toml` с `[toolchain] channel = "1.84.x"` (или текущий) и заменить `@stable` на `@${{ env.RUST_VERSION }}` либо просто на читалку файла. Дополнительно — pin SHA для всех `uses:` (сейчас все `@v4`, `@v2` — мутабельные tag'и).

### MEDIUM (UX/maintenance)

#### M1. Backup-файлы collide в одну секунду

- Файл: `bosun-client/crates/bosun-primitives/src/file_content/backup.rs:35`.
- Timestamp `%Y%m%dT%H%M%SZ` — точность секунды. Два apply подряд внутри одной секунды (CI или агрессивный reconcile) перетрут старый backup через `fs::copy` (overwrites).
- L1 review-1 не закрыт. Решение: добавить наносекунды в имя или `OpenOptions::new().create_new(true)` с инкрементным suffix'ом при конфликте.

#### M2. M5 review-1 не закрыт: Diff::Unknown отсутствует

- Файл: `bosun-client/crates/bosun-core/src/diff.rs:7-18`.
- `Diff` имеет только `NoChange`, `Add`, `Update`. `runr.service`/`systemd.service` без status snapshot отдают `Update` — dry-run на смешанной ноде, где runr daemon выкл, постоянно репортит drift. Оператор не отличает реальный drift от «status недоступен».
- Решение: ввести `Diff::Unknown { reason: String }`, в orchestrator считать его как `unknown` в summary (не drift, не no_change), и в CLI/text-output печатать отдельным маркером.

#### M3. `bundle validate` пишет «evaluate OK» даже когда тэги не активированы

- Файл: `bosun-client/crates/bosun-cli/src/bundle_validate.rs:96-99`.
- При `tags.require_one_of("production")` и `--tags ""` валидация падает с EVAL_ERROR — это правильно. Но при `tags.has("production")` ветке внутри которой условный `apt.package`, validate пропустит её без проверки payload'а — потому что без активного тэга примитив не зарегистрируется. Оператор увидит «OK», а на ноде с правильным тэгом упадёт.
- Решение: для validate-режима либо предупреждение «evaluate без активных тэгов проверил только X из Y роли», либо требовать `--tags` обязательно.

#### M4. ApplyCtx прирос published_facts mutex'ом без observability

- Файл: `bosun-client/crates/bosun-core/src/primitive.rs:138`.
- `published_facts: Arc<Mutex<HashMap<String, serde_json::Value>>>` — каждый `pg_sql.query store_as_fact` ставит туда. Нет метрики «сколько фактов опубликовано», нет лога при перезаписи, нет ограничения на размер value. Bundle с багом `pg_sql.query` каждой роли публикующий 10MB JSON приведёт к OOM без явного сигнала.
- Решение: structured-log `tracing::info!(fact=name, prev_present=..)` при `publish_fact`, метрика `bosun_published_facts_total` и hard-cap на размер JSON-payload (например, 64KB).

#### M5. pub surface крейтов слишком широкий

- Файл: `bosun-client/crates/bosun-core/src/lib.rs:3-27`, `bosun-client/crates/bosun-primitives/src/lib.rs:8-29`.
- `pub mod tracing_test_util;` — test helper без `#[cfg(any(test, feature = "test-util"))]` gate'а. Все 21 модулей `bosun-primitives` тоже `pub mod`. Любой внешний код (например, bosun-server в будущем) может зацепиться за internal API.
- L2 review-2 не закрыт. Решение: gate `tracing_test_util` за feature `test-util`; в каждом примитиве сделать `pub mod` только то, что нужно re-export'нуть. Остальное — `pub(crate)`.

#### M6. Operator runbook не покрывает первичную установку на голую ноду

- Файл: `bosun-client/docs/operator-runbook.md:14-39`.
- Раздел «Установка» предлагает `cargo install --path crates/bosun-cli`, что требует Rust toolchain на ноде. Apt-секция говорит «сборка вне репозитория». Musl-binary упомянут, но нет ссылки на release artifact или способа доставки на голую ноду без интернета.
- E (test-coverage-audit:7) findings: «Quick start не сработает на не-разработчике». Документация остаётся developer-focused.
- Решение: добавить «Из GitHub Release» секцию — `curl -fsSLO https://github.com/.../releases/download/v0.1.0/bosun-x86_64-musl && install -m0755 ...`. Это требует чтобы tag-based musl-build реально создавал release (уже есть в `musl-build.yml:48`), оператор узнаёт куда смотреть.

#### M7. Cross-bundle notify identity не описан в Status quo

- Файл: `bosun-client/docs/operator-runbook.md:408-410`.
- В Status quo сказано «Cross-bundle notify identity отсутствует» — но это блокер для PG-стека, где postgres-bundle и pgbouncer-bundle живут отдельно. Один bundle не может попросить рестарт сервиса из другого.
- Это не баг, а design gap. Решение: либо документировать workaround (один bundle с подролями), либо добавить future-spec link.

#### M8. Метрики bosun_defers_executed_total/replay_total — counter в overwritten textfile

- Файл: `bosun-client/crates/bosun-cli/src/metric.rs:163-188`.
- TYPE counter, но файл перезаписывается атомарно каждый run. Prometheus `increase()` увидит drop при перезаписи и репортит negative deltas (которые тихо игнорятся). M3 review-2 не закрыт.
- Решение: либо переименовать в `bosun_defers_executed_last_run_total` (gauge), либо хранить cumulative counter в `--state-dir/defers-counter.json` и инкрементировать при загрузке.

### LOW (nice-to-have)

#### L1. README:3 badge URL содержит typo

- Файл: `bosun-client/README.md:3`.
- `https://github.com/vadv/bonsun/actions/...` — `bonsun`, не `bosun`. Если репозиторий называется `bosun`, badge'и не отображаются.

#### L2. Метрика bosun_active_tags имеет только текстовый кардиналитет

- Файл: `bosun-client/crates/bosun-cli/src/tags_metric.rs` (по README:254).
- `bosun_active_tags{tag="<name>"} 1` — кардиналитет растёт линейно с числом тэгов. Если оператор по ошибке передал uuid в `--tags`, метрика взорвёт Prometheus storage.
- Решение: либо whitelist через `bundle.toml [tags]`, либо hard-cap на число label values.

#### L3. process.signal не имеет idempotency note для нескольких процессов

- Файл: `bosun-client/docs/bundle-authoring.md:751-769`.
- Документация говорит «отправить сигнал процессу через pkill». При нескольких процессах под `process_name` сигнал получат все, и `pkill -0` (probe) тоже даёт OK при первом. Не описано — это intended или баг.

#### L4. nginx-demo и multi-role-pg не имеют `cd bosun-client` в начале README

- Файлы: `bosun-client/examples/nginx-demo/README.md:9`, `examples/multi-role-pg/README.md:109`.
- L2 review-1 не закрыт. Commands предполагают cwd=bosun-client; копи-паст с другой cwd ломается.

#### L5. tracing_test_util доступен извне теста

- Файл: `bosun-client/crates/bosun-core/src/lib.rs:27`.
- См. M5. Технически это test fixture, но `pub` без gate'а позволяет включить его в production-сборку.

## Что особенно хорошо

1. `defers/journal.rs:130-136` — `Journal::open` через `O_NOFOLLOW`+`fchmod`+`fstat`-проверка владельца. Хирургически закрытый B4 без переноса journal'а на `/var/lib/bosun` (сохранили tmpfs-semantics). Тесты прямо имитируют атаку через symlink на чужой файл (`open_rejects_symlink_to_sensitive_file_does_not_chmod_target`) — это уровень defense-in-depth ниже не часто видишь.
2. Cancellation/deadline разнесены на `Outcome::Interrupted` отдельно от `Outcome::Deferred` (`orchestrator.rs:546-565`). Tests `dry_run_replay_gate::dry_run_true_skips_replay_and_keeps_journal_intact` — регрессия H1 review-2 закрыта системно: `maybe_run_replay_phase` гейтит обе фазы pre/post.
3. UnitName newtype с regex-валидацией `^[a-zA-Z0-9][a-zA-Z0-9._@-]*$` и max 255 байт (`unit_name.rs`). M3 review-1 закрыт. Это пример «security as type».
4. `dispatch.rs:179-232` — `run_with_timeout` с polling 50ms, kill+wait при таймауте. Comment подчёркивает связь с regression H4 review-2 — обработка зависших deferred команд. Хорошее обоснование почему 60s константа, и почему `--deadline-sec` тут не помогает.
5. `bosun_last_attempt_timestamp_seconds` + comment «alert on staleness here, not on bosun_last_run» (`metric.rs:83`). Операционная грамотность — алертить на attempt, а не на success. Это редко встречается в Prometheus-метриках.
6. read-before-write принцип закреплён в memory `feedback_bosun_read_before_write_principle.md` и реально применяется: `users.user` использует `nix::unistd::User::from_name` перед `useradd`, `file.symlink` — `read_link` перед remove+symlink. Тесты mutation-counter'а есть.
7. `Bundle::check_compatibility` против `cargo-pkg-version` через `requires_bosun = "^0.1"` (по README:71). Реальная защита от запуска несовместимого bundle'а.
8. `published_facts` через `OverlayFactsSource` (`primitive.rs:534-565`) — runtime published facts работают через стандартный `FactsSource::get`, без отдельного API. Симметричный дизайн.
9. Anti-cache policy для runr/systemd snapshots (`primitive.rs:66-72` comment + memory `feedback_bosun_no_cache_for_runr_systemd`). Узнали грабли Phase D — откатили. Это редкое явление: команда явно отказалась от оптимизации после анализа.
10. 1418 unit/integration тестов + 107 BDD сценариев. Особенно `dispatch.rs::run_with_timeout_kills_long_running_command` — реальное измерение wall-clock против таймаута. Регрессия H4 review-2 ловится этим тестом.

## Что игнорим (по решениям user'а)

- **Self-upgrade** (memory `feedback_bosun_no_self_upgrade.md`, 2026-05-20). bosun не автообновляется; обновление через apt-package + центральный push. Не упоминаем в findings.
- **Secrets handling в bundle.toml**. `pg_sql.exec` с паролем в открытом виде в bundle (`bundle-authoring.md:783`) — известный gap. Vault-integration на стороне bosun-server.
- **runr daemon как dependency без инструкции**. Pgbouncer-cluster требует `127.0.0.1:8010`, но откуда взять runr — out of scope для bosun-client.
- **Установка на голую ноду**. Apt-пакет и release-артефакт — операционная ответственность.
- **PG discovery-факты** (pg_is_master, pg_users) — server-side scope.
- **Bundle distribution** (S3/OCI/HTTPS-pull) — следующая итерация, bosun-server.
- **bosun connect (gRPC mode)** — живёт в bosun-server репозитории.

## Suggested fix order

1. **Этой неделей.** H1 (LC_ALL=C в RealCommandRunner — однострочный фикс с большим impact). H4 (rename metric или add probe — `daemon_info()` уже есть). H7 (rust-toolchain.toml).
2. **До production.** H2 (BDD в CI — отдельный job, медленный, on-demand для PR). H3 (real build_payload и real template_fn в bundle validate). H6 (parent chain symlink в file.content).
3. **После H1-H7.** H5 (deadline в pacer + orchestrator). M1 (backup nanoseconds). M2 (Diff::Unknown). M4 (published_facts observability + cap).
4. **Документация.** M3 (validate warning без тэгов). M6 (release-artifact в operator-runbook). L1 (typo в badge URL).
5. **Cleanup.** M5 (curated pub surface, gate test-util). M8 (defers counter semantics: gauge vs persistent).

## Open questions

1. **LC_ALL=C тестируется как?** BDD пускать с `LANG=ru_RU.UTF-8` намерено сложно — нужен docker-image с локалью. Unit-тест через mock CommandRunner ловит только argv, не env. Как acceptance для H1 — argv builder тест + integration через docker?
2. **Diff::Unknown** в plan-фазе runr.service — это change в публичной trait'е Primitive. Сериализация в JSON-отчёте — новый variant. Затронет bosun-server gRPC schema. Решать сейчас или после server-side?
3. **bundle validate full render mode** требует фактов. Сейчас fixture (`--facts ./fixtures/...`) поддерживается, но inv (`inventory.read`) рендерится из bundle. Если template зависит от обоих — fixture inv тоже нужен. Стоит ли это «full mode» CLI флаг или отдельная команда?
4. **published_facts hard-cap** — если operator реально хочет публиковать большие данные (например, список всех PG ролей с MD5-хешами), 64KB может оказаться мало. Делать конфигурируемым через CLI или жёстко?
5. **deadline propagation через cancel** — если переходить на «deadline cancel'ит токен», то Outcome::Interrupted должен различать «SIGTERM» и «deadline». Сейчас reason - строка, разрозненный. Нужен отдельный enum?
6. **pacer без cancel-обработки deadline'а** — если CLI не cancel'ит токен при deadline, нужно ли это менять в run.rs (spawn задача, ждёт `tokio::time::sleep_until(deadline)`, cancel'ит) или в orchestrator (явная проверка `Instant::now() >= deadline`)? Первый — изменение flow, второй — изменение orchestrator-логики.
7. **BDD в CI cost** — 107 сценариев в docker, build test-base image, run cucumber — ~10-15 минут на ubuntu-22.04. Платить на каждый push на main или только nightly + PR-on-label?
