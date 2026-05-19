# Bundle authoring guide

Руководство для автора bundle'ов. Покрывает layout, Starlark API, все 21
примитив, notify-каналы, validation, health-check, теги и факты.
Заканчивается отсылкой к `examples/pgbouncer-cluster/` — полному
production-like примеру с reload по изменению конфига.

## Что такое bundle

Bundle — это директория с манифестом на Starlark, шаблонами Jinja2 и
yaml-инвентарём. CLI читает её путём, evaluate'ит `main.star`, регистрирует
ресурсы, применяет в топологическом порядке. Никаких MANIFEST, никаких
.tar.gz, никаких подписей в текущей итерации — просто директория.

## Layout

```
mybundle/
├── bundle.toml                # манифест: name, version, requires_bosun, tags
├── main.star                  # entry point в корне
├── inventory/                 # yaml-данные, иерархия на усмотрение автора
│   ├── base.yaml
│   ├── production.yaml
│   └── staging.yaml
├── roles/                     # роли
│   └── <role>/
│       ├── main.star          # экспортирует функции (configure, install, ...)
│       └── templates/         # Jinja2-шаблоны ТОЛЬКО этой роли
│           └── *.j2
├── _lib/                      # опционально: shared helpers
│   └── <lib>/
│       ├── main.star          # экспортирует функции
│       └── templates/         # шаблоны lib'а
│           └── *.j2
└── fixtures/                  # опционально: JSON-факты для `bundle validate`
    └── facts-*.json
```

Правила:

- `bundle.toml` обязателен в корне.
- `main.star` в корне — единственный entry point.
- `inventory/` — произвольная иерархия. Структура не важна для
  bosun-core: загрузку определяет `inventory.read()` из манифеста.
- `roles/<name>/main.star` обязателен для каждой роли.
- `_lib/<name>/main.star` обязателен для каждой lib.
- Несоответствие директории `_lib/` и load-namespace'а `@lib/`
  намеренное: `_` — сигнал «не сканируйте как роль», `@lib/` — короткий
  префикс для load.

### `bundle.toml`

```toml
[bundle]
name           = "pgbouncer-cluster"
version        = "0.1.0"
description    = "pgbouncer на одной ноде с runr-supervised сервисом"
requires_bosun = "^0.1"
# entry        = "main.star"     # опционально, дефолт "main.star"

[bundle.inventory]
default_merge_strategy = "deep_map_replace_list"

[bundle.tags]
production = "Production cluster"
staging    = "Staging cluster"
canary     = "Canary subset for risky changes"
```

`requires_bosun` — semver-req против `cargo-pkg-version`. `^0.1` означает
`>=0.1.0, <0.2.0`. Для bundle'ов, рассчитанных на минорные апгрейды,
используйте `>=0.1` без каретки.

`default_merge_strategy` подхватывается `inventory.merge()`, когда не
указан явный `strategy=`. Значения: `deep_map_replace_list`,
`deep_map_append_list`, `replace`.

`[bundle.tags]` — документация для `--help`. Во время evaluate список не
валидируется: CLI принимает любые тэги, bundle сам решает что делать через
`tags.has()` / `tags.require_one_of()`.

## Starlark basics

### `load`

Поддерживаемые префиксы:

| Префикс | Резолвится в | Когда |
|---|---|---|
| `@bosun/builtins` | виртуальный модуль с native-globals | везде |
| `@roles/<name>` | `roles/<name>/main.star` | везде |
| `@lib/<name>` | `_lib/<name>/main.star` | везде |

Примеры:

```starlark
load("@bosun/builtins", "apt", "file", "service", "template")
load("@roles/postgres", configure_postgres = "configure")
load("@lib/runr", "render_service", "render_timer")
```

Символы, начинающиеся с `_`, приватны для модуля — внешний `load` их не
видит (Bazel-convention).

Циклические импорты падают на parse-этапе с диагностической ошибкой.

### `inventory`

```starlark
load("@bosun/builtins", "inventory")

base = inventory.read("inventory/base.yaml")
overrides = inventory.read("inventory/production.yaml")
inv = inventory.merge(base, overrides)

# Альтернатива для списков объектов, объединяемых по ключу:
servers = inventory.merge_keyed(inv1, inv2, key = "id")
```

`inventory.read(path)` читает yaml относительно корня bundle, возвращает
Starlark dict. Кешируется в пределах одного evaluate.

`inventory.merge(*sources, strategy=?)` сливает map'ы. Стратегии:

- `deep_map_replace_list` — глубокий merge map'ов, для list'ов правый
  источник заменяет полностью.
- `deep_map_append_list` — глубокий merge, list'ы конкатятся с
  дедупликацией по equality.
- `replace` — правый источник заменяет всё.

`null` в правом источнике на любом уровне удаляет ключ из левого.

`inventory.merge_keyed(*sources, key=)` ожидает, что любой list внутри
источников — список объектов с ключом `<key>`. Элементы объединяются по
совпадению значения этого ключа.

### `tags`

```starlark
tags.has("production")          # bool
tags.require_one_of("production", "staging")   # fail-fast если нет
tags.active()                    # отсортированный list[str] для логов
```

`tags.require_one_of(...)` рекомендуется вызывать в начале `main.star`,
сразу после `load`-блоков. Это даёт fail-fast при кривом `--tags=`.

### `template(path, **kwargs)`

Рендерит Jinja2-шаблон **рядом с модулем, в котором определена вызывающая
функция**. Внутри функции из `roles/postgres/main.star` → `roles/postgres/templates/<path>`.
Внутри функции из `_lib/runr/main.star` → `_lib/runr/templates/<path>`.

Вызов из корневого `main.star` запрещён — корень это orchestration, не
content. Cross-module access (`template("@roles/foo:x.j2")`) тоже запрещён.

kwargs передаются в Jinja-контекст:

```starlark
file.content(
    path = "/etc/nginx/nginx.conf",
    contents = template("nginx.conf.j2", inv = inv, env = "prod"),
    ...,
)
```

В шаблоне доступны `{{ inv.worker_processes }}` и `{{ env }}`.

## Notify-каналы: `depends_on`, `reload_on`, `restart_on`

Каждый ресурс-управленец сервиса (`service.unit`, `runr.service`,
`systemd.service`, `runr.timer`, `systemd.timer`, `sysctl.reload`) принимает
до трёх списков handle'ов. Они выглядят похоже, но семантика разная:
`depends_on` — это ordering (когда и в каком порядке), `reload_on` /
`restart_on` — notify (что сделать, если родитель действительно поменялся).

### `depends_on` — строгий порядок

Топологический сорт регистра гарантирует, что parent применится до child.
Если parent упал (`Outcome::Failed`) или был прерван (`Outcome::Interrupted`),
любой child с `depends_on=[parent]` получает `Outcome::Skipped` БЕЗ
запуска primitive. Это распространяется транзитивно: внук тоже пропускается.
Поведение не зависит от `--continue-on-error` — флаг управляет только тем,
останавливать ли весь прогон, не транзитивный skip.

`depends_on` не триггерит defers. Это просто barrier «не трогай меня,
пока эти не отработают».

Типичный случай: unit-файл сервиса должен лечь на диск до того, как мы
просим runr/systemd этот сервис запустить.

```starlark
unit_file = file.content(
    path = "/etc/runr/pgbouncer.service",
    contents = render_service(...),
)
service.unit(
    name = "pgbouncer", state = "running",
    depends_on = [unit_file],   # без unit'а старт бессмысленный
)
```

### `reload_on` — notify-канал, soft

Если хотя бы один из перечисленных handle'ов в этом прогоне реально
изменился (`Outcome::Changed`), в журнал defers ставится `reload:<target>`.
Если parent изменился, но потом упал на пост-валидации, или вообще не
менялся (`Outcome::Unchanged`) — defer не ставится.

`reload_on` НЕ влияет на ordering. Если parent ещё не применён в момент
вызова child'а, это будет race. Поэтому правило: handle, перечисленный в
`reload_on`, обычно перечисляется и в `depends_on` — это и логично
(не reload'ить раньше изменения), и безопасно с точки зрения сборки плана.

Failure parent'а в `reload_on` НЕ skip'ает child: child всё равно
apply'нется (например, sync-state «running» подтвердится), просто без
триггера reload.

### `restart_on` — то же, но Restart

Эквивалент `reload_on`, ставит в defers `restart:<target>` вместо
`reload:<target>`. Restart субсумирует Reload в дедуп-логике (см. ниже):
если в журнале уже лежит `restart:pgbouncer`, новый `reload:pgbouncer`
становится no-op'ом.

### Таблица поведения

| Аспект | `depends_on` | `reload_on` | `restart_on` |
|---|---|---|---|
| Влияет на топ-сорт | да | нет | нет |
| Parent.Failed → child | Skipped без apply | apply без defer | apply без defer |
| Parent.Changed → action | ничего | defer `reload:` | defer `restart:` |
| Parent.Unchanged → action | ничего | ничего | ничего |
| Дедуп внутри прогона | n/a | по `(action, target)` | по `(action, target)` |
| Дедуп между прогонами | n/a | restart субсумирует reload | restart субсумирует reload |

### Когда что выбирать

- Изменился `pgbouncer.ini` — pgbouncer должен перечитать конфиг без обрыва
  соединений: `reload_on = [pgbouncer_config_handle]`.
- Изменился `pgbouncer.service` (unit-файл runr/systemd) — нужен restart,
  иначе новый ExecStart не подхватится: `restart_on = [unit_handle]`.
- `pgbouncer` стартует от unit-файла — unit должен лежать до старта:
  `depends_on = [unit_handle]`.

Полный bundle с тройной семантикой:

```starlark
unit_file = file.content(
    path = "/etc/runr/pgbouncer.service",
    contents = render_service(...),
)
pgb_ini = file.content(
    path = "/etc/pgbouncer/pgbouncer.ini",
    contents = template("pgbouncer.ini.j2", inv = inv),
    validate_with = ["pgbouncer", "-V", "-c", "{new_path}"],
)
service.unit(
    name = "pgbouncer", state = "running", enable = True,
    depends_on = [unit_file],
    reload_on  = [pgb_ini],
    restart_on = [unit_file],
)
```

В одном прогоне defers дедуплицируются по `(action, target, init_system)`.
Если изменились и `pgbouncer.ini`, и `userlist.txt` — будет ровно один
`reload:pgbouncer`.

## Defers — журнал отложенных действий

Defers — это журнал на tmpfs (`/tmp/bosun-defers/` в проде), куда bosun
складывает действия, которые должны выполниться после успешного apply'я:
рестарты, релоады, deferred-сигналы. Журнал переживает запуски bosun'а,
но не reboot — это by design.

### Зачем

Сценарий: bosun меняет `pgbouncer.ini`, нужно перечитать конфиг. Если
делать reload прямо в момент записи файла — будет лавина из 12 reload'ов,
когда в одном прогоне меняются и ini, и userlist.txt, и unit-файл, и
sysctl. Поэтому действия откладываются в журнал и схлопываются в одно.

Второй сценарий: bosun упал между записью файла и reload'ом (OOM, kill,
паника). Без журнала reload потерян — оператор видит свежий конфиг и
сервис, работающий по старому. С журналом запись `reload:pgbouncer`
лежит на диске; следующий apply начинается с replay, дотягивает reload.
Это и есть at-least-once семантика.

### Lifecycle одной записи

1. **Enqueue.** Primitive (например `service.unit`) видит, что один из
   handle'ов в `restart_on` изменился, вызывает `ctx.defers.enqueue(...)`
   с `DeferEntry { action, target, init_system, ... }`.
2. **Atomic write.** Journal пишет файл атомарно: `tmp` → `fsync(file)` →
   `rename` → `fsync(dir)`. Имя файла — `<sortkey>-<id>.deferred`. После
   rename запись видна другим читателям, включая следующий запуск bosun'а
   после crash'а.
3. **Replay.** В конце apply'я (post-replay) журнал сканируется ещё раз:
   `list_sorted()` возвращает все `.deferred` лексикографически. Pre-replay
   фаза в начале следующего прогона делает то же самое — это safety net,
   если предыдущий процесс упал между enqueue и post-replay.
4. **Dispatch.** Для каждой записи replay вызывает `DispatchClient::run`,
   который маршрутизирует по `init_system`: `systemd` → dbus к
   `org.freedesktop.systemd1`, `runr` → HTTP к runr daemon, `command` →
   spawn argv.
5. **Success.** Запись `unlink`'ается + `fsync(dir)`. Если у записи был
   `health_check` — он гоняется до unlink'а; failure health-check'а
   считается за неудачный replay.
6. **Failure.** `journal.bump_attempt` инкрементирует `attempt_count`,
   перезаписывает файл атомарно. Если `attempt_count >= max_attempts`
   (дефолт 3, флаг `--defer-max-attempts`) — `move_to_manual_clear`:
   файл переименовывается в `<id>.manual_clear`, replay его игнорирует
   до ручного вмешательства.

### Priority и порядок

Имя файла начинается с двухсимвольного префикса, синхронного с
`DeferPriority::sortkey()`. Lex-сортировка `read_dir`'а сразу даёт
правильный порядок выполнения:

| Префикс | Action | Когда |
|---|---|---|
| `0r` | `Restart` | первый |
| `1r` | `ReloadOrRestart` | |
| `2r` | `Reload` | |
| `3c` | `Command` (отложенный shell/argv) | после service-actions |
| `4d` | `DaemonReload` | последний |

Числовые префиксы выбраны намеренно: при `c0/r0/r1/...` лексическая
сортировка ставит `c0 < r0` и `command.run` поехал бы раньше service-
actions, что противоречит спеке.

### Dedup правила

Внутри одного `(target, init_system)`:

- **Восходящая субсумация.** Уже есть `Restart`, прилетел `Reload` или
  `ReloadOrRestart` — новая запись становится `EnqueueResult::Subsumed`
  (no-op). Restart покрывает reload, держать обе записи бессмысленно.
- **Нисходящая замена.** Уже есть `Reload`, прилетел `Restart` — старая
  запись удаляется, новая пишется. Результат — `ReplacedLowerPriority`.
- **Идемпотентность.** Тот же `id` (target + init_system + action) при
  повторном enqueue — `AlreadyExists`, content файла не меняется, поля
  `enqueued_at` и `enqueued_by` стабильны.

Разные `init_system` (`systemd-pgbouncer` vs `runr-pgbouncer`) не
дедуплицируются между собой — это два независимых target'а.

### Почему tmpfs

`/tmp` — tmpfs, ребут обнуляет директорию. Это by design:

- После reboot все managed-сервисы стартуют заново через init-систему.
  Накопленный `reload:nginx` неактуален — nginx уже только что
  поднялся со свежим конфигом.
- tmpfs убирает дисковый I/O из критической секции apply'я — `fsync(dir)`
  на tmpfs — это lock + memory barrier, не запись на блочное устройство.

Если operational reason требует persistent journal (тестовая нода, debug
crash-сценариев), `--defers-dir` принимает любой путь — например,
`/var/tmp/bosun-defers`. Цена — после reboot replay будет дотягивать
бесполезные reload'ы.

### Метрики и видимость

- `bosun_defers_pending` — gauge, сколько `*.deferred` в журнале сейчас.
- `bosun_defers_executed_total{result=ok|failed|client_unavailable|manual_clear}` —
  counter по результатам replay'я. Алерт на `manual_clear` сигнализирует,
  что operator-attention нужен.
- `bosun_defers_replay_total` — счётчик replay-фаз за прогон (обычно 2:
  pre + post).
- `bosun status` печатает текущий journal в виде таблицы. `bosun status
  --clear <id>` удаляет одну запись (сначала `*.deferred`, потом
  `*.manual_clear`). `--clear-all-manual` сносит всё, что застряло.

## Validation pattern: `validate_with`

Чтобы сломанный конфиг не попал к live-сервису, `file.content` принимает
параметр `validate_with` — argv валидатора, который запускается на временной
копии до replace.

### Зачем это нужно

Без валидатора последовательность такая: bosun перезаписывает
`/etc/nginx/nginx.conf` → notify в defers → replay делает `systemctl reload
nginx` → nginx говорит «syntax error», старый процесс продолжает работать
со старой конфигурацией. Результат: drift между файлом и runtime,
непредсказуемое поведение после следующего перезапуска ноды.

С валидатором bosun запускает `nginx -t -c /etc/nginx/nginx.conf.new` на
временной копии. Exit 0 → swap, exit != 0 → конфиг live не трогается,
оператор видит ошибку и работающий nginx в исходном состоянии.

### Lifecycle на `file.content` с валидатором

1. Compute `sha256(new_contents)`. Если совпадает с текущим — `NoChange`,
   валидатор не запускается, никаких side effects.
2. Рендеринг содержимого в `<path>.new` (атомарно через tempfile +
   rename внутри той же директории).
3. Substitute `{new_path}` в `argv` → `["pgbouncer", "-V", "-c",
   "/etc/pgbouncer/pgbouncer.ini.new"]`.
4. Spawn argv через `Command::new` (никакого `sh -c`, никаких глоббингов).
   Polling `try_wait` каждые 50 мс до `timeout` (дефолт 30s).
5. Exit 0 → `rename(<path>.new, <path>)` → `fsync(dir)` → `record_changed`.
6. Exit != 0 → `PrimitiveError::Validation { exit_code, stderr_excerpt }`.
   `<path>.new` **остаётся на диске** для forensics (можно сделать diff
   и понять, что bundle сгенерил неправильно). `<path>` не изменён.
7. Timeout → процесс убит, `PrimitiveError::Validation { kind=Timeout }`.

Пример:

```starlark
file.content(
    path = "/etc/pgbouncer/pgbouncer.ini",
    contents = template("pgbouncer.ini.j2", inv = inv),
    validate_with = ["pgbouncer", "-V", "-c", "{new_path}"],
    owner = "postgres",
    group = "postgres",
    mode = 0o640,
)
```

### Lifecycle на `service.unit` (без файла)

Валидатор запускается **перед** enqueue defer'а. Failure → defer не
ставится, ресурс отдаёт `Outcome::Failed`, апстрим узнаёт об ошибке
синхронно. Это важно: без pre-check'а сломанный конфиг попадёт в defer,
replay будет долбить `nginx reload` до `manual_clear`, оператор разбирает
post-mortem.

### Substitution rules

Подменяется ровно один плейсхолдер — `{new_path}`. Все остальные
конструкции `{...}` остаются как есть: bosun не пытается парсить
произвольный syntax вроде `{path}`, `{owner}`, `{checksum}`. Это
сознательное ограничение — известная поверхность substitution не
расширяется без явного RFC.

Плейсхолдер может встречаться внутри аргумента (`--config={new_path}`)
и в нескольких аргументах одновременно. Если аргументов с `{new_path}`
нет вообще — валидатор не получит файл и упадёт на собственной диагностике.

## Health-check

`service.unit`, `runr.service`, `systemd.service` принимают `health_check_cmd`
или `health_check_url` (взаимоисключающие). Это последняя линия защиты:
init-система отвечает «started», но сервис может слушать порт и отвечать
500-кой на каждом запросе. Health-check проверяет реальный статус.

### Зачем

Без health-check'а: bosun делает `systemctl restart nginx`, systemd
репортит `active (running)`, defer успешно snimается, метрики говорят
«всё ок». Реально nginx загрузил кривой `mime.types`, отвечает 500-кой.
Bosun этого не замечает, оператор узнаёт через мониторинг endpoint'ов.

С health-check'ом после restart bosun спрашивает у сервиса
`pg_isready` / `curl /healthz`. Не отвечает за retry × interval — defer
помечается failed, attempt инкрементируется, при исчерпании попыток
запись уезжает в `.manual_clear` с явным сигналом.

### Sync на Start

Когда `state=running` и сервис ранее не работал (`is-active inactive`),
после `service_start` сразу гоняется health-check синхронно. Это
блокирующая часть apply'я: пока probe не вернул success или не исчерпались
retry'и, ресурс не закрыт.

Failure → `PrimitiveError::HealthCheckFailed { reason }`. Defer не
создаётся (нечего реload'ить, сервис не должен был запускаться вообще).
Topo-skip propagate'ится: ресурсы с `depends_on=[failed_service]`
получают `Outcome::Skipped`.

### Async для Restart/Reload (через defer-replay)

Сценарий с notify-каналом: parent (config) изменился → ставится
`restart:<service>` в журнал → defer-replay подхватывает запись →
делает action (restart) → запускает health-check той же спецификации,
что лежит в defer-entry.

Health-check failure здесь:

- Запись остаётся `.deferred`.
- `attempt_count += 1` через `journal.bump_attempt`.
- Если `attempt_count >= max_attempts` (дефолт 3) → промоушен в
  `.manual_clear`. Следующие replay-фазы запись игнорируют.

### Retry policy

| Параметр | Дефолт | Что значит |
|---|---|---|
| `health_check_retry` | 3 | Сколько попыток подряд. |
| `health_check_retry_interval_sec` | 2 | Sleep между попытками. |
| `health_check_timeout_sec` | 10 | Дедлайн одной попытки. |

Между попытками runner проверяет `CancellationToken`: SIGTERM или
`--deadline-sec` дают `HealthCheckError::Cancelled` (не failure — отдельный
класс, чтобы replay не bump'ал attempt при штатном завершении).

### Cmd vs Url

- `health_check_cmd = ["pg_isready", "-h", "127.0.0.1", "-p", "6432"]` —
  spawn argv, exit 0 = healthy. stderr попадает в excerpt (4096 байт)
  для post-mortem'а.
- `health_check_url = "http://127.0.0.1:6432/healthz"` — GET через
  `ureq`. По умолчанию ожидается код 200, перебивается
  `health_check_expected_status = 204`.

Пример с обоими стилями:

```starlark
service.unit(
    name = "pgbouncer",
    state = "running",
    reload_on = [pgbouncer_config],
    health_check_cmd = ["pg_isready", "-h", "127.0.0.1", "-p", "6432"],
    health_check_retry = 5,
    health_check_retry_interval_sec = 3,
    health_check_timeout_sec = 10,
)

service.unit(
    name = "nginx",
    state = "running",
    reload_on = [nginx_config],
    health_check_url = "http://127.0.0.1/healthz",
    health_check_expected_status = 200,
)
```

## Tags и canary

Один bundle, несколько окружений. Тэги — единственный механизм ветвления
поведения bundle'а без отдельных bundle-репозиториев.

### CLI и активный набор

`--tags=production,canary-low` принимается как CSV. CLI дедуплицирует и
сортирует список до передачи в evaluator, поэтому
`--tags=canary-low,production,production` и `--tags=production,canary-low`
дают идентичный прогон. Активный набор — `BTreeSet<String>`, неизменяемый
во время evaluate.

### API

- `tags.has(name) -> bool` — проверка одного тэга. Часто используется
  для условных веток конфигурации.
- `tags.require_one_of(*names) -> None` — fail-fast. Если ни один из
  перечисленных тэгов не активен, evaluate падает с диагностической
  ошибкой `tags: expected one of [production, staging] in active set,
  got [empty]`. Exit 3.
- `tags.active() -> list[str]` — отсортированная копия активного набора.
  Подходит для логирования и для метрик.

### Слоистая конфигурация

Типичный паттерн «base + env-overrides + canary-tweak»:

```starlark
load("@bosun/builtins", "inventory", "tags")
load("@roles/postgres", configure_postgres = "configure")

tags.require_one_of("production", "staging")

base = inventory.read("inventory/base.yaml")
if tags.has("production"):
    inv = inventory.merge(base, inventory.read("inventory/production.yaml"))
else:
    inv = inventory.merge(base, inventory.read("inventory/staging.yaml"))

# Канарейка дополнительно подкручивает несколько параметров
if tags.has("canary"):
    inv = inventory.merge(inv, inventory.read("inventory/canary-overrides.yaml"))

configure_postgres(inv = inv)
```

### Severity-классы для канареечного rollout'а

Раскатать рискованное изменение постепенно: 1% парка → 5% → 25% → 100%.
Идея — оператор повышает severity-class в bundle.toml, ноды распределяются
по классам через стабильный hash от hostname'а. Псевдо-Starlark:

```starlark
# Каждая нода получает severity_class из inventory (выставляется
# deployment-системой по hash hostname'а). Bundle ниже срабатывает на
# тэге, выставляемом оркестратором по этому классу.

if tags.has("canary-1pct") and inv["severity_class"] == 0:
    enable_risky_feature(inv)
elif tags.has("canary-5pct") and inv["severity_class"] <= 1:
    enable_risky_feature(inv)
elif tags.has("canary-25pct") and inv["severity_class"] <= 4:
    enable_risky_feature(inv)
elif tags.has("production"):
    enable_risky_feature(inv)
```

Этот код — оператор-side: bosun даёт примитивы для условной активации,
сам hash и распределение по severity-классам — responsibility
deployment-системы (bosun-server, ansible, что угодно). На уровне ноды
bosun видит только финальный CLI-флаг `--tags=...`.

### Метрика для дебага

`bosun.prom` пишется один; отдельно агент создаёт `bosun_tags.prom` с
гейджем `bosun_active_tags{tag="<name>"} 1` для каждого активного тэга.
Алерт «нода в production без тэга production» строится прямо на этой
метрике без необходимости парсить логи.

## Facts

Факты — данные о ноде, собранные перед evaluate. Доступны примитивам через
`ApplyCtx`, а не напрямую в Starlark (Starlark видит только то, что отдают
`inventory.read()` и API namespace'ов).

Каждый факт имеет одно из трёх состояний:

- `Known(value)` — собран, доступен.
- `Unknown { reason }` — собрать не удалось. Примитивы, которые от него
  зависят, должны явно решать что делать.
- `Stale { value, age_ms }` — старое значение, новое собрать не удалось.
  Используется примитивом на свой страх (например, `installed_packages`
  после крэша dpkg).

Собираемые факты:

| Имя | Тип | Что значит |
|---|---|---|
| `hostname` | str | FQDN ноды. |
| `init_system` | str | `systemd` / `runr` / `mixed-systemd-runr` / `unknown`. Влияет на `service.unit` диспатч. |
| `cpu_count` | int | cgroup-aware число CPU. |
| `memory_mb` | int | cgroup-aware total memory. |
| `is_pod` | bool | True, если процесс под systemd-pod / cgroup namespace. |
| `installed_packages` | map | dpkg-state: установленные пакеты с версиями. Lazy: помечается dirty после `apt.package`. |

Состояние каждого факта попадает в `bosun_fact_state{fact="<name>"}`:
`0=Known`, `1=Unknown`, `2=Stale`. Это нужно для алерта «у 5% парка
init_system Unknown — что-то поломали в коллекторе».

## Каталог примитивов

21 примитив, сгруппированных по namespace'у. Жирные параметры обязательны;
остальные — опциональные с дефолтами в круглых скобках.

### `apt.*`

`apt.package(name=, state="present", version=?, timeout_sec=?, allow_downgrade=False, allow_change_held=False)`

Установить/удалить пакет. `state` — `present` / `absent` / `purged`. Пин
версии — `version=`. Идемпотентность через факт `installed_packages`:
если нужная версия стоит, apt не вызывается. Восстанавливается из
half-configured dpkg-state (повторный `dpkg --configure -a`).

```starlark
apt.package(name = "nginx", version = "1.22.1-9")
apt.package(name = "snapd", state = "absent")
```

`apt.update_cache(name=, max_age_sec=3600, force=False, cleanup_old_debs_days=1, skip_cleanup=False)`

Ленивый `apt-get update`: skip, если `/var/cache/apt/pkgcache.bin` моложе
`max_age_sec`. После обновления удаляет `.deb` в `archives/` старше
`cleanup_old_debs_days` дней.

`apt.key(name=, state=, url=?|key_data=?, fingerprint=?, keyring_path="/etc/apt/keyrings/<name>.gpg")`

GPG-ключ репозитория в modern signed-by стиле. Источник — HTTP URL либо
inline `key_data`. Если задан `fingerprint`, ключ верифицируется через
`gpg --show-keys`. Legacy `apt-key add` сознательно не поддерживается.

### `file.*`

`file.content(path=, contents=, owner=?, group=?, mode=?, validate_with=?)`

Атомарная запись через `tempfile + rename`. Backup исходного файла в
`--backup-dir`. Симлинки целевого пути отвергаются (refusing-to-follow).
`validate_with` — argv валидатора, `{new_path}` подставляется (см. секцию
Validation pattern).

```starlark
file.content(
    path = "/etc/nginx/nginx.conf",
    contents = template("nginx.conf.j2", inv = inv),
    owner = "root", group = "root", mode = 0o644,
    validate_with = ["nginx", "-t", "-c", "{new_path}"],
)
```

`file.delete(path=, recursive=False, follow_symlinks=False)` — удаление
файла, симлинка или директории. Симлинки удаляются как символические
ссылки без следования за target.

`file.symlink(path=, target=, state="present", force=False)` — управление
симлинком. `target` может пока не существовать (chiit-кейс с симлинками
PG до раскатки дистрибутива). `force=True` разрешает заменить
существующий файл/директорию.

### `users.*`

`users.user(name=, state=, uid=?, group=?, shell=?, home=?, no_create_home=False, system=False, comment=?)`

Декларативный системный пользователь. Под капотом — `useradd` / `usermod`
/ `userdel`. Если spec совпадает с фактом — no-op. Требует root.

```starlark
users.user(
    name = "pgbouncer", state = "present", system = True,
    shell = "/usr/sbin/nologin", home = "/var/lib/pgbouncer",
)
```

`users.group(name=, state=, gid=?, system=False)` — то же для группы. При
расхождении GID — `groupmod --gid`.

### `service.unit` (диспатчер)

`service.unit(name=, state=, enable=?, reload_on=?, restart_on=?, depends_on=?, health_check_cmd=?|health_check_url=?, health_check_*=?, validate_with=?)`

Абстрактный диспатчер. Читает факт `init_system`: для `systemd` /
`mixed-systemd-runr` идёт в `systemd.service`, для `runr` — в
`runr.service`. Init-специфичные поля (например, `cgroup_procs_path`)
отвергаются — для них используйте `runr.service` / `systemd.service`
напрямую. `state` — `running` / `stopped` / `absent`.

### `runr.*`

`runr.service(name=, state=, enable=False, ...)` — прямой контроль runr
unit'а через HTTP API. Кроме общего поднабора `service.unit` принимает
runr-специфичные поля: `cgroup_procs_path`, `restart_policy` и прочие.
`state` — `running` / `stopped` / `absent`.

`runr.timer(name=, state=, enable=?, start_now=False, ...)` — runr-таймер.
`state` — `enabled` / `disabled` / `absent`.

`runr.cgroup(name=, state=, ...)` — конфигурирует cpu/memory/io limits
на уровне cgroup в runr. `state` — `present` / `absent`.

### `systemd.*`

`systemd.service(name=, state=, enable=True, ...)` — контроль systemd
unit'а через dbus к `org.freedesktop.systemd1`. Кроме общего поднабора
принимает systemd-специфичные поля: `condition_path_exists`, drop-in
override и т. п. `state` — `running` / `stopped` / `absent`. `enable`
дефолтится в `true` (типичный systemd-стиль), в отличие от `runr.service`.

`systemd.timer(name=, state=, enable=True, ...)` — systemd-таймер.
`state` — `enabled` / `disabled` / `absent`.

### `process.signal`

`process.signal(name=, signal=, process_name=?|process_user=?, deferred=True)`

Отправить allowlist-сигнал процессу через `pkill --signal`. Сигналы
ограничены `HUP` / `TERM` / `INT` / `USR1` / `USR2` / `WINCH` / `PIPE`.
`KILL` / `STOP` / `CONT` сознательно отсутствуют — для остановки
процессов используйте `service.unit(state="stopped")`.

Селектор — ровно один из `process_name` (имя бинаря) или `process_user`
(владелец). По дефолту `deferred=True`: запись попадает в журнал defers
и выполняется в replay-фазе.

```starlark
process.signal(
    name = "hup-pg-doorman", signal = "HUP",
    process_name = "pg_doorman",
)
```

### `pg_sql.*`

`pg_sql.exec(name=, dsn=, sql=, if_not_exists_check=?, timeout_sec=?)`

Выполнить DDL/DML/GRANT через sync PG-клиент. Идемпотентность —
опциональный `if_not_exists_check`: если SELECT возвращает >0 строк,
exec пропускается.

```starlark
pg_sql.exec(
    name = "create-monitoring-role",
    dsn = "postgresql://postgres@127.0.0.1:5432/postgres",
    sql = "CREATE ROLE monitoring WITH LOGIN PASSWORD 'changeme'",
    if_not_exists_check = "SELECT 1 FROM pg_roles WHERE rolname = 'monitoring'",
)
```

`pg_sql.query(name=, dsn=, sql=, timeout_sec=?, store_as_fact=?)` —
SELECT. При `store_as_fact=<name>` результат публикуется в runtime-registry
фактов, доступен последующим примитивам через `ApplyCtx::read_published_fact`.
Результат — список maps `{column → value}` (все значения строкой).

### `cert.tls`

`cert.tls(cert_path=, key_path=, common_name=, algorithm="rsa2048", days_valid=3650, renew_before_days=30, owner=?, group=?, mode_cert=0o644, mode_key=0o600, subject_alt_names=[])`

Self-signed x509-сертификат через pure-Rust pipeline (rcgen + ring + rsa-крейт).
Без openssl-binary и libssl. `algorithm` — `rsa2048` / `ed25519` /
`ecdsa_p256`. Перевыпуск, когда до expiry осталось меньше `renew_before_days`.

### `sysctl.reload`

`sysctl.reload(name=, path=)` — `sysctl -p <path>` для одного `.conf`-файла.
Plan всегда отдаёт Update (ядро не сообщает, когда последний раз грузили
файл), но повторный set того же значения через `sysctl -p` — no-op на
уровне ядра. Apply падает, если `path` не существует. Композиция:

```starlark
sysctl_conf = file.content(
    path = "/etc/sysctl.d/99-postgres.conf",
    contents = "vm.overcommit_memory = 2\nvm.swappiness = 1\n",
)
sysctl.reload(
    name = "postgres-tuning",
    path = "/etc/sysctl.d/99-postgres.conf",
    depends_on = [sysctl_conf],
    reload_on = [sysctl_conf],
)
```

### `template` (pure-функция)

`template(path, **kwargs) -> str` — не ресурс, а pure-функция. Рендерит
Jinja2-шаблон рядом с модулем, в котором определена вызывающая функция,
и возвращает строку. См. секцию Starlark basics. Типичный паттерн —
`file.content(contents = template("nginx.conf.j2", inv = inv))`.

## Полный пример

Все три элемента — слоистый inventory, lib-композиция, cross-resource notify
с health-check'ом — собраны в `examples/pgbouncer-cluster/`. Скелет роли:

```starlark
load("@bosun/builtins", "apt", "file", "service", "template")
load("@lib/runr", "render_service")

def configure(inv):
    apt.package(name = "pgbouncer")

    pgbouncer_config = file.content(
        path = "/etc/pgbouncer/pgbouncer.ini",
        contents = template("pgbouncer.ini.j2", inv = inv),
        owner = "postgres", group = "postgres", mode = 0o640,
        validate_with = ["pgbouncer", "-V", "-c", "{new_path}"],
    )
    pgbouncer_users = file.content(
        path = "/etc/pgbouncer/userlist.txt",
        contents = template("userlist.txt.j2", inv = inv),
        owner = "postgres", group = "postgres", mode = 0o640,
    )
    runr_unit = file.content(
        path = "/etc/runr/pgbouncer.service",
        contents = render_service(
            name = "pgbouncer",
            exec_start = "/usr/sbin/pgbouncer /etc/pgbouncer/pgbouncer.ini",
            user = "postgres", group = "postgres",
            autostart = True, limit_nofile = 65536,
        ),
        mode = 0o644,
    )

    service.unit(
        name = "pgbouncer", state = "running", enable = True,
        depends_on = [runr_unit],
        reload_on  = [pgbouncer_config, pgbouncer_users],
        restart_on = [runr_unit],
        health_check_cmd = ["pg_isready", "-h", "127.0.0.1", "-p", "6432"],
        health_check_retry = 5,
        health_check_retry_interval_sec = 3,
    )
```

Это даёт at-least-once reload с pre-replace валидатором конфига и
health-check'ом. При crash bosun'а между записью `.ini` и reload — defer
остаётся на диске, следующий apply начнётся с pre-replay и доведёт reload
до конца. Полный bundle с inventory и шаблонами лежит в
`examples/pgbouncer-cluster/bundle/`, разбор по строкам — в его README.
