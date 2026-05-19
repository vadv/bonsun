# multi-role-pg

Кластер PostgreSQL (postgres + Patroni + pgbouncer) с раздельными ролями
и общим `_lib/runr/` для генерации unit-файлов. Цель — показать на
нетривиальной структуре, как класть многоролевой bundle: numbered-prefix
inventory, lib-композиция, module-relative `template()`.

Шаблоны намеренно упрощены — пример для проверки формата, не для реальной
production-раскатки Postgres.

## Что делает example

1. Загружает `inventory/base.yaml`, затем по тэгу `production`/`staging`
   merge'ит `inventory/<env>.yaml`, затем три файла из
   `inventory/postgresql/`.
2. Через три роли (`postgres`, `patroni`, `pgbouncer`) ставит пакеты
   и пишет конфиги.
3. Роль `postgres` импортирует `render_service` из `_lib/runr/` и кладёт
   рендереный systemd unit-файл.

## Prerequisites

- Debian-based нода под root.
- apt-кеш доступен, репозитории PostgreSQL подключены (либо все имена
  пакетов из `inventory/postgresql/01_packages.yaml` доступны из default-репов
  Debian/Ubuntu).
- На ноде нет конфликтующих установок postgres/patroni/pgbouncer; иначе
  apply попытается заменить версии.
- Журнал defers пуст до старта.
- Для smoke-теста — Docker с образом `bosun-test-base:latest`.

## Команды для запуска

Apply на staging:

    bosun apply --bundle ./bundle --tags=staging \
        --lock-path /tmp/bosun.lock \
        --state-dir /tmp/bosun-state \
        --log-dir /tmp/bosun-log \
        --backup-dir /tmp/bosun-backups \
        --metric-file /tmp/bosun.prom

Apply на production:

    bosun apply --bundle ./bundle --tags=production

Без `--tags` или с неизвестным значением CLI возвращает exit 3.

Валидация без обращения к системе (для CI bundle-репозитория и pre-commit
hook'ов):

    bosun bundle validate --bundle ./bundle --tags=staging

Smoke-test в Docker:

    make bosun-bookworm
    docker run --rm -v $(pwd):/work bosun-test-base:latest sh -c '
        cp /work/target/bookworm-release/bosun /usr/local/bin/bosun
        bosun bundle validate \
            --bundle /work/examples/multi-role-pg/bundle \
            --tags=staging
    '

## Expected outcome

После `bosun apply` на чистой ноде:

1. Установлен пакет `postgresql-15` (или версия из inventory).
2. Файлы `/etc/postgresql/15/main/postgresql.conf` и `pg_hba.conf` записаны.
3. Установлен `patroni`, файл `/etc/patroni/patroni.yml` записан.
4. Установлен `pgbouncer`, файл `/etc/pgbouncer/pgbouncer.ini` записан.
5. systemd unit-файл `/etc/systemd/system/postgres-runr.service` (или
   аналогичный путь из роли) записан через render_service.
6. Exit 0. Метрика: `bosun_resources_total{outcome="changed"} >= 6`
   на первом прогоне; повторный — все unchanged.

Под капотом данный пример **не управляет состоянием сервисов** (нет
`service.unit`-вызовов в ролях) — он демонстрирует только структуру bundle
и сборку конфигов. Для production-раскатки добавьте `service.unit` с
`reload_on`/`restart_on` (см. `pgbouncer-cluster` пример).

## Verification

    bosun bundle validate --bundle ./bundle --tags=staging
    # evaluate OK, N resources registered

    bosun apply --bundle ./bundle --tags=staging --dry-run
    # exit 0 если нода уже в нужном состоянии

    dpkg-query -W postgresql-15 patroni pgbouncer
    cat /etc/postgresql/15/main/postgresql.conf

## Структура

    bundle/
    ├── bundle.toml
    ├── main.star
    ├── inventory/
    │   ├── base.yaml
    │   ├── production.yaml
    │   ├── staging.yaml
    │   └── postgresql/
    │       ├── 01_packages.yaml
    │       ├── 02_install.yaml
    │       └── 03_config.yaml
    ├── roles/
    │   ├── postgres/{main.star, templates/{postgresql.conf.j2, pg_hba.conf.j2}}
    │   ├── patroni/{main.star, templates/patroni.yml.j2}
    │   └── pgbouncer/{main.star, templates/pgbouncer.ini.j2}
    └── _lib/runr/
        ├── main.star                    # render_service(name, exec_start, restart, user)
        └── templates/service.j2

## Inventory chaining

`main.star` сливает inventory в таком порядке:

    base.yaml
      → postgresql/01_packages.yaml
      → postgresql/02_install.yaml
      → postgresql/03_config.yaml
      → production.yaml | staging.yaml (по тэгу)

Стратегия по умолчанию — `deep_map_replace_list` из `bundle.toml`.
Последний источник побеждает по ключам, list'ы заменяются целиком.

## Composition `roles/postgres` → `_lib/runr`

Роль `postgres` импортирует `render_service` из `@lib/runr` и зовёт её
для генерации unit-файла:

    load("@lib/runr", "render_service")

    file.content(
        path = "/etc/systemd/system/postgres-runr.service",
        contents = render_service(name = "postgres", exec_start = "..."),
        ...,
    )

`render_service` внутри `_lib/runr/main.star` зовёт
`template("service.j2", ...)`. Module-relative резолв находит шаблон в
`_lib/runr/templates/service.j2`, а не в `roles/postgres/templates/`. Это
показывает isolation между ролями и lib.
