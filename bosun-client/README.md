# bosun-client

bosun-client — SCM-агент на Rust. Запускается под root, читает декларативный
bundle на Starlark, собирает факты о ноде, считает план и применяет ресурсы
с advisory-lock'ом. Аналог chef/puppet/ansible, заточенный под парк
PostgreSQL-нод: пакеты, конфиги, сертификаты, systemd/runr-юниты, отложенные
рестарты с health-check'ом.

Один бинарь — два режима:

- `bosun apply --bundle=...` — локальный apply на ноде. Для разработки
  bundle'ов, отладки инцидентов, CI, bootstrap'а ноды до подключения к
  серверу.
- `bosun connect <bosun-server>` — production-режим под управлением сервера
  через gRPC (живёт отдельно в репозитории bosun-server; в этом крейте пока
  только локальный apply).

## Quick start

Соберите бинарь из исходников:

    cargo install --path crates/bosun-cli

Бинарь `bosun` встанет в `~/.cargo/bin/bosun`. Скопируйте на ноду:

    install -m 0755 ~/.cargo/bin/bosun /usr/local/bin/bosun

Для деплоя на парк нод собирайте статический musl-бинарь — без зависимостей
от glibc, openssl, libdbus, libpq:

    make musl-x86_64      # x86_64-unknown-linux-musl, нужен musl-tools
    make musl-aarch64     # aarch64-unknown-linux-musl
    make musl-docker      # воспроизводимая сборка в rust:alpine
    make musl-verify      # smoke-test в distroless/static контейнере

Результат — `target/x86_64-unknown-linux-musl/release/bosun`, ~25 MB,
`ldd` показывает `statically linked`. Запускается на любом Linux с
ядром >= 3.2, в том числе distroless/static и scratch.

Создайте минимальный bundle:

    mkdir -p hello/roles/motd/templates
    cat > hello/bundle.toml <<'EOF'
    [bundle]
    name           = "hello"
    version        = "0.1.0"
    requires_bosun = "^0.1"

    [bundle.inventory]
    default_merge_strategy = "deep_map_replace_list"

    [bundle.tags]
    production = "main env"
    EOF

    cat > hello/main.star <<'EOF'
    load("@bosun/builtins", "tags")
    load("@roles/motd", configure_motd = "configure")

    tags.require_one_of("production")
    configure_motd()
    EOF

    cat > hello/roles/motd/main.star <<'EOF'
    load("@bosun/builtins", "file", "template")

    def configure():
        file.content(
            path = "/etc/motd",
            contents = template("motd.j2"),
            mode = 0o644,
            owner = "root",
            group = "root",
        )
    EOF

    cat > hello/roles/motd/templates/motd.j2 <<'EOF'
    Managed by bosun.
    EOF

Запустите dry-run, чтобы увидеть план без записи на диск:

    sudo bosun apply --bundle hello --tags=production --dry-run

В выводе будет `file.content:/etc/motd  drift  +N bytes` и exit 2 —
есть pending changes. Запустите без `--dry-run`, чтобы применить:

    sudo bosun apply --bundle hello --tags=production

Exit 0 — `/etc/motd` записан. Повторный запуск выдаст exit 0 без drift'а:
агент идемпотентен.

Следующий шаг — посмотреть на `examples/nginx-demo/`, `examples/multi-role-pg/`
и `examples/pgbouncer-cluster/`. Последний — реалистичный production-like
случай: пакет, конфиг с валидатором, runr/systemd unit, notify-driven reload
через журнал defers.

## Commands

### `bosun apply`

Применить bundle к локальной системе. Главный режим для разработки и для
самостоятельного запуска на ноде.

    bosun apply --bundle ./bundle --tags=production
    bosun apply --bundle ./bundle --tags=staging --dry-run
    bosun apply --bundle ./bundle --tags=canary --log-level=debug --log-format=json

При apply агент берёт advisory-lock `/var/run/bosun.lock` (другая инстанция
вернёт exit 0 без действия), собирает факты, evaluate'ит `main.star`,
строит топ-сорт ресурсов, применяет каждый, после успешного apply'я делает
replay defer-журнала, пишет Prometheus textfile-метрику.

### `bosun bundle validate`

Статически проверить bundle без обращения к системе. Не читает факты,
не пишет файлы. Используется в CI bundle-репозитория и pre-commit hook'ах.

    bosun bundle validate --bundle ./bundle --tags=production
    bosun bundle validate --bundle ./bundle --tags=staging --facts ./fixtures/facts-runr.json

Опциональный `--facts <path>` подсовывает JSON-факты вместо реального
сбора — это нужно для случаев, когда bundle зависит от факта (`service.unit`
диспатчит по `init_system`). Без `--facts` ресурсы, требующие фактов,
завершают валидацию с диагностической ошибкой.

Вывод при успехе: `evaluate OK, N resources registered`, exit 0.

### `bosun status`

Показать состояние журнала defers — отложенных действий, которые agent
запомнил после изменения зависимого ресурса (типично: «pgbouncer.ini
изменился — reload pgbouncer»).

    bosun status
    bosun status --format=json
    bosun status --clear systemd.restart:nginx.service
    bosun status --clear-all-manual

Без аргументов печатает таблицу:

    ID                     STATE          ACTION   TARGET     ATTEMPTS  ENQUEUED_AT
    systemd.reload:nginx   pending        reload   nginx      0         2026-05-19T10:15:00Z
    systemd.restart:pgb    manual_clear   restart  pgbouncer  3         2026-05-19T09:55:12Z

`--clear <id>` удаляет один файл (сначала ищет `*.deferred`, потом
`*.manual_clear`). `--clear-all-manual` сносит все зависшие записи —
типичный оператор-кейс после разбирательства с инцидентом. Exit 0, если
journal чист или содержит только pending; exit 1, если есть
`*.manual_clear`.

### `bosun version`

Печатает версию бинаря и exit 0.

## CLI флаги

Сначала общие флаги `bosun apply`. Дефолты подобраны под production
(абсолютные пути под root); в Docker и CI обычно нужны overrides.

| Флаг | Дефолт | Что делает |
|---|---|---|
| `--bundle <PATH>` | (required) | Путь к директории bundle'а. |
| `--tags <CSV>` | `[]` | Активные тэги (`production,canary`). CLI дедупит и сортирует. |
| `--dry-run` | `false` | Прогнать план без записи. Exit 2 — есть drift. |
| `--continue-on-error` | `false` | Не останавливаться на первой ошибке ресурса. |
| `--log-level <L>` | `info` | `debug` / `info` / `warn` / `error`. |
| `--log-format <F>` | `text` | `text` или `json` (для коллекторов). |
| `--format <F>` | `text` | Формат отчёта на stdout: `text` или `json`. |
| `--no-color` | `false` | Без ANSI-цветов в текстовом отчёте. |
| `--lock-path <P>` | `/var/run/bosun.lock` | Файл advisory-lock'а через `flock`. |
| `--deadline-sec <N>` | `600` | Глобальный deadline на весь прогон. По истечении — SIGTERM-семантика, exit 130. |
| `--state-dir <P>` | `/var/lib/bosun` | Persistent state агента (backups index, мелочи). |
| `--log-dir <P>` | `/var/log/bosun` | Логи прогонов. |
| `--backup-dir <P>` | `/var/backups/bosun` | Куда `file.content` складывает бэкапы перед replace. |
| `--metric-file <P>` | `/var/lib/node_exporter/textfile_collector/bosun.prom` | Prometheus textfile-collector. |
| `--runr-url <URL>` | `http://127.0.0.1:8010` | Базовый URL runr-демона. Используется, если факт `init_system` — `runr` или `mixed-systemd-runr`. |
| `--runr-timeout-sec <N>` | `10` | Таймаут одного HTTP-вызова runr. |
| `--defers-dir <P>` | `/tmp/bosun-defers` | Корень журнала defer'ов (tmpfs by design). |
| `--defer-max-attempts <N>` | `3` | Сколько раз replay пытается выполнить запись, прежде чем промоутит в `.manual_clear`. |

Флаги `bosun status`:

| Флаг | Дефолт | Что делает |
|---|---|---|
| `--defers-dir <P>` | `/tmp/bosun-defers` | Какой journal читать (должен совпадать с тем, что у `apply`). |
| `--format <F>` | `text` | `text` (таблица) или `json` (массив объектов). |
| `--clear <ID>` | — | Удалить один defer или manual_clear по канонический id или по имени файла. |
| `--clear-all-manual` | `false` | Снести все `*.manual_clear`. |

## Exit codes

| Code | Смысл |
|---|---|
| `0` | Apply без ошибок (включая «всё уже стоит»); либо `--dry-run` без drift'а; либо advisory-lock занят (другая инстанция активна). |
| `1` | Apply начался, часть ресурсов применилась, потом случилась критическая ошибка ресурса. Либо `bosun status` нашёл `*.manual_clear`. |
| `2` | `--dry-run` обнаружил drift — есть pending changes. Для CI-gating. |
| `3` | Ошибка до apply: невалидный manifest, отсутствует ключ inv, version mismatch `requires_bosun`, bundle не загрузился, `tags.require_one_of` не сработал. |
| `4` | CLI/окружение: некорректные аргументы, не удалось создать state/log/backup-директории, не удалось открыть lock-файл. |
| `130` | Прогон прерван SIGTERM/SIGINT или истёк `--deadline-sec`. POSIX-стандарт `128 + SIGINT`. |

## Метрики Prometheus

Агент после прогона пишет атомарно (через `rename`) `bosun.prom` —
textfile для node_exporter'а. Метрики:

| Метрика | Тип | Что показывает |
|---|---|---|
| `bosun_last_attempt_timestamp_seconds{version}` | gauge | UTC время последнего инвокейшна. Алертить на staleness именно её, не `last_run`. |
| `bosun_last_run_timestamp_seconds{version}` | gauge | UTC время последнего успешно завершённого прогона. |
| `bosun_last_run_exit_code` | gauge | Exit-код последнего прогона. |
| `bosun_last_run_duration_seconds` | gauge | Длительность последнего прогона в секундах. |
| `bosun_resources_total{outcome}` | gauge | Per-outcome счётчик ресурсов: `changed`, `unchanged`, `failed`, `deferred`, `interrupted`. |
| `bosun_fact_state{fact}` | gauge | Per-fact состояние: `0=Known`, `1=Unknown`, `2=Stale`. |
| `bosun_defers_pending` | gauge | Сколько `*.deferred` лежит в journal'е сейчас. |
| `bosun_defers_executed_total{result}` | counter | Per-replay результат: `ok`, `failed`, `client_unavailable`, `manual_clear`. |
| `bosun_defers_replay_total` | counter | Сколько replay-фаз прошло за прогон. |
| `bosun_runr_reachable` | gauge | `1`, если runr-handle сконструирован в этот прогон. |
| `bosun_systemd_reachable` | gauge | `1`, если systemd-handle сконструирован. |

Отдельный файл `bosun_tags.prom` содержит `bosun_active_tags{tag} 1` для
каждого активного CLI-тэга. Используется для отладки «какой тэг приехал
на эту ноду».

## Документация

- [docs/bundle-authoring.md](docs/bundle-authoring.md) — как написать bundle.
  Layout, Starlark API, каталог 21 примитива, notify-каналы, validation,
  health-check, tags, facts, пример роли pgbouncer.
- [docs/operator-runbook.md](docs/operator-runbook.md) — как деплоить и
  эксплуатировать. Установка, кронtab/таймер, lock-файл, дебаг провального
  apply'я, recovery, defers semantics, известные ограничения.
- [examples/nginx-demo/](examples/nginx-demo/README.md) — минимальный bundle.
- [examples/multi-role-pg/](examples/multi-role-pg/README.md) — три роли
  + shared `_lib/runr/`.
- [examples/pgbouncer-cluster/](examples/pgbouncer-cluster/README.md) —
  cross-resource notify, validate_with, health-check, defers replay.

## Структура крейтов

- `crates/bosun-core` — типы (`Resource`, `ResourceId`, `Registry`), контракты
  примитивов (`Primitive`, `FactsSource`), Starlark-evaluator,
  Bundle-loader, Orchestrator, defers journal.
- `crates/bosun-facts` — коллекторы фактов с trait'ом `Fact` и
  явным состоянием `Known/Unknown/Stale`.
- `crates/bosun-primitives` — реализации 21 примитива.
- `crates/bosun-runr-client` — sync HTTP-клиент к runr daemon (ureq).
- `crates/bosun-systemd-client` — async dbus-клиент к systemd1 через zbus,
  sync-фасад для примитивов.
- `crates/bosun-handles` — общая инфраструктура runtime-хэндлов.
- `crates/bosun-cli` — бинарь `bosun`.

## Сборка и тесты

    make build        # cargo build --release
    make test         # workspace lib/bins/tests
    make fmt          # cargo fmt --all -- --check
    make clippy       # workspace clippy с deny-warnings
    make test-bdd     # BDD в Docker (требует docker)
    make test-bdd TAGS=@apt-package    # только сценарии с тэгом
