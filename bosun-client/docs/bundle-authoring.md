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

    load("@bosun/builtins", "apt", "file", "service", "template")
    load("@roles/postgres", configure_postgres = "configure")
    load("@lib/runr", "render_service", "render_timer")

Символы, начинающиеся с `_`, приватны для модуля — внешний `load` их не
видит (Bazel-convention).

Циклические импорты падают на parse-этапе с диагностической ошибкой.

### `inventory`

    load("@bosun/builtins", "inventory")

    base = inventory.read("inventory/base.yaml")
    overrides = inventory.read("inventory/production.yaml")
    inv = inventory.merge(base, overrides)

    # Альтернатива для списков объектов, объединяемых по ключу:
    servers = inventory.merge_keyed(inv1, inv2, key = "id")

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

    tags.has("production")          # bool
    tags.require_one_of("production", "staging")   # fail-fast если нет
    tags.active()                    # отсортированный list[str] для логов

`tags.require_one_of(...)` рекомендуется вызывать в начале `main.star`,
сразу после `load`-блоков. Это даёт fail-fast при кривом `--tags=`.

### `template(path, **kwargs)`

Рендерит Jinja2-шаблон **рядом с модулем, в котором определена вызывающая
функция**. Внутри функции из `roles/postgres/main.star` → `roles/postgres/templates/<path>`.
Внутри функции из `_lib/runr/main.star` → `_lib/runr/templates/<path>`.

Вызов из корневого `main.star` запрещён — корень это orchestration, не
content. Cross-module access (`template("@roles/foo:x.j2")`) тоже запрещён.

kwargs передаются в Jinja-контекст:

    file.content(
        path = "/etc/nginx/nginx.conf",
        contents = template("nginx.conf.j2", inv = inv, env = "prod"),
        ...,
    )

В шаблоне доступны `{{ inv.worker_processes }}` и `{{ env }}`.

## Notify-каналы: `depends_on`, `reload_on`, `restart_on`

Каждый ресурс-управленец сервиса (`service.unit`, `runr.service`,
`systemd.service`, `runr.timer`, `systemd.timer`, `sysctl.reload`) принимает
до трёх списков handle'ов:

- `depends_on` — ordering: ресурс не применяется, пока перечисленные не
  применены. Не триггерит никаких defer'ов сам по себе.
- `reload_on` — если хотя бы один из перечисленных handle'ов реально
  изменился (Outcome::Changed) в этом прогоне, добавляется defer
  `reload:<target>`.
- `restart_on` — то же, но `restart:<target>`. Restart субсумирует reload:
  если есть оба, остаётся только restart.

Когда что использовать:

- Изменился `pgbouncer.ini` — pgbouncer должен перечитать конфиг без обрыва
  соединений. `reload_on = [pgbouncer_config_handle]`.
- Изменился `pgbouncer.service` (unit-файл runr/systemd) — нужен restart,
  иначе новый ExecStart не подхватится. `restart_on = [unit_handle]`.
- `pgbouncer` стартует от unit-файла, поэтому unit должен быть положен
  до старта: `depends_on = [unit_handle]`.

Внутри одного apply'я defers дедуплицируются по `(action, target)`. Если
изменились и `pgbouncer.ini`, и `userlist.txt` — будет ровно один
`reload:pgbouncer`.

## Defers

Defers — отложенные действия на tmpfs, выполняемые в replay-фазе. Каждый
defer — JSON-файл в `/tmp/bosun-defers/<id>.deferred`. Журнал
переживает crash bosun'а: если процесс упал между записью конфига и
рестартом, следующий apply начнётся с replay'я и доведёт reload до конца
(at-least-once семантика).

Жизненный цикл записи:

1. **Создаётся** во время apply'я, когда `reload_on` / `restart_on`
   срабатывает или `process.signal(deferred=True)` регистрирует сигнал.
2. **Pre-replay** — в начале следующего apply'я journal сканируется,
   каждая запись выполняется до evaluate-фазы.
3. **Post-replay** — после успешного apply'я journal сканируется ещё
   раз: записи этого прогона выполняются здесь.
4. **Promote в `.manual_clear`** — после `--defer-max-attempts` (дефолт 3)
   неудач файл переименовывается в `<id>.manual_clear`. Дальше replay
   его игнорирует — нужен оператор.

Reboot стирает `/tmp/bosun-defers/` — это by design. Системные сервисы
после reboot стартуют сами, accumulated reload'ы прошлой жизни ноды
неактуальны.

Файлы дефолтно лежат в `/tmp/bosun-defers/`, путь перебивается флагом
`--defers-dir`. На staging-ноде можно положить в `/var/tmp/bosun-defers`,
если tmpfs-`/tmp/` слишком маленький.

`bosun status` печатает текущий journal. `bosun status --clear <id>`
удаляет одну запись; `--clear-all-manual` сносит все зависшие после ручного
разбора.

## Validation pattern: `validate_with`

Чтобы сломанный конфиг не попал к live-сервису, `file.content` принимает
параметр `validate_with` — argv валидатора, который запускается на временной
копии до replace:

    file.content(
        path = "/etc/pgbouncer/pgbouncer.ini",
        contents = template("pgbouncer.ini.j2", inv = inv),
        validate_with = ["pgbouncer", "-V", "-c", "{new_path}"],
        owner = "postgres",
        group = "postgres",
        mode = 0o640,
    )

Поведение:

1. Контент пишется в `/etc/pgbouncer/pgbouncer.ini.new`.
2. Запускается `pgbouncer -V -c /etc/pgbouncer/pgbouncer.ini.new`.
3. Exit 0 → atomic rename `.new` → live-путь, defer добавляется в journal.
4. Exit != 0 → `.new` остаётся на диске для forensics, `.ini` не трогается,
   ресурс получает Outcome::Failed, defer **не** добавляется.

Плейсхолдер `{new_path}` подменяется на путь временного файла. Без него
валидатор не получит файл для проверки.

То же поле есть на `service.unit`, `runr.service`, `systemd.service`,
`file.symlink`: валидатор запускается перед применением.

## Health-check

`service.unit`, `runr.service`, `systemd.service` принимают `health_check_cmd`
или `health_check_url` (взаимоисключающие):

    service.unit(
        name = "pgbouncer",
        state = "running",
        reload_on = [pgbouncer_config],
        health_check_cmd = ["pg_isready", "-h", "127.0.0.1", "-p", "6432"],
        health_check_retry = 5,
        health_check_retry_interval_sec = 3,
        health_check_timeout_sec = 30,
    )

Когда вызывается:

- **Sync на Start.** При первом старте сервиса (state=running, ранее
  не работал) health-check выполняется синхронно. Не прошёл за retry × interval
  — ресурс Failed.
- **Async через replay для Reload/Restart.** Когда defer выполнился, replay
  отдельно гоняет health-check. Не прошёл — defer считается failed, attempt
  увеличивается, при достижении max — promote в `.manual_clear`.

Для `health_check_url`:

    health_check_url = "http://127.0.0.1:6432/healthz",
    health_check_expected_status = 200,    # дефолт 200

Для `health_check_cmd` — exit 0 = healthy.

## Tags и canary

Один bundle, несколько окружений. Тэги — единственный механизм ветвления:

    # main.star
    load("@bosun/builtins", "inventory", "tags")
    load("@roles/postgres", configure_postgres = "configure")

    tags.require_one_of("production", "staging")

    base = inventory.read("inventory/base.yaml")
    if tags.has("production"):
        inv = inventory.merge(base, inventory.read("inventory/production.yaml"))
    else:
        inv = inventory.merge(base, inventory.read("inventory/staging.yaml"))

    configure_postgres(inv = inv)

    # Дополнительно: что-то применяется только на canary-нодах.
    if tags.has("canary"):
        load("@roles/canary_probes", install_probes = "install")
        install_probes()

CLI принимает `--tags=production,canary` — список CSV. CLI дедупит и
сортирует перед передачей в evaluator.

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

    apt.package(name = "nginx", version = "1.22.1-9")
    apt.package(name = "snapd", state = "absent")

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

    file.content(
        path = "/etc/nginx/nginx.conf",
        contents = template("nginx.conf.j2", inv = inv),
        owner = "root", group = "root", mode = 0o644,
        validate_with = ["nginx", "-t", "-c", "{new_path}"],
    )

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

    users.user(
        name = "pgbouncer", state = "present", system = True,
        shell = "/usr/sbin/nologin", home = "/var/lib/pgbouncer",
    )

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

    process.signal(
        name = "hup-pg-doorman", signal = "HUP",
        process_name = "pg_doorman",
    )

### `pg_sql.*`

`pg_sql.exec(name=, dsn=, sql=, if_not_exists_check=?, timeout_sec=?)`

Выполнить DDL/DML/GRANT через sync PG-клиент. Идемпотентность —
опциональный `if_not_exists_check`: если SELECT возвращает >0 строк,
exec пропускается.

    pg_sql.exec(
        name = "create-monitoring-role",
        dsn = "postgresql://postgres@127.0.0.1:5432/postgres",
        sql = "CREATE ROLE monitoring WITH LOGIN PASSWORD 'changeme'",
        if_not_exists_check = "SELECT 1 FROM pg_roles WHERE rolname = 'monitoring'",
    )

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

### `template` (pure-функция)

`template(path, **kwargs) -> str` — не ресурс, а pure-функция. Рендерит
Jinja2-шаблон рядом с модулем, в котором определена вызывающая функция,
и возвращает строку. См. секцию Starlark basics. Типичный паттерн —
`file.content(contents = template("nginx.conf.j2", inv = inv))`.

## Полный пример

Все три элемента — слоистый inventory, lib-композиция, cross-resource notify
с health-check'ом — собраны в `examples/pgbouncer-cluster/`. Скелет роли:

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

Это даёт at-least-once reload с pre-replace валидатором конфига и
health-check'ом. При crash bosun'а между записью `.ini` и reload — defer
остаётся на диске, следующий apply начнётся с pre-replay и доведёт reload
до конца. Полный bundle с inventory и шаблонами лежит в
`examples/pgbouncer-cluster/bundle/`, разбор по строкам — в его README.
