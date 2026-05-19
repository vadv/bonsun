# multi-role-pg

Реалистичный bundle bosun-client'а формата rev 2 — кластер Postgres
(postgres + Patroni + pgbouncer) с раздельными ролями и общим
`_lib/runr/` для генерации systemd unit-файлов.

Цель примера — показать на не-тривиальной структуре:

- numbered prefix inventory (`postgresql/01_packages.yaml`,
  `02_install.yaml`, `03_config.yaml`) + env-overlay
  (`production.yaml` / `staging.yaml`);
- три роли (`postgres`, `patroni`, `pgbouncer`), каждая со своими
  шаблонами;
- `_lib/runr/` экспортирует `render_service(...)`, который вызывается
  из роли `postgres` и возвращает rendered unit-файл;
- module-relative `template()` — шаблоны живут рядом с ролью или lib,
  cross-module access запрещён;
- `tags.require_one_of("production", "staging")` в начале манифеста.

Шаблоны намеренно упрощены — пример служит для проверки формата bundle,
а не как реальный production-конфиг Postgres.

## Запуск

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

## Валидация без обращения к системе

    bosun bundle validate --bundle ./bundle --tags=staging

Печатает `evaluate OK, N resources registered` или диагностику с
exit 3. Применимо для CI bundle-репозитория и pre-commit hook.

## Структура

    bundle/
    ├── bundle.toml                              # name, version, requires_bosun, tags, inventory.default_merge_strategy
    ├── manifests/main.star                      # entry: загружает inventory + три роли
    ├── inventory/
    │   ├── base.yaml                            # timezone, locale, cluster-wide settings
    │   ├── production.yaml                      # overrides для tags=production
    │   ├── staging.yaml                         # overrides для tags=staging
    │   └── postgresql/
    │       ├── 01_packages.yaml                 # postgres_package, postgres_package_version
    │       ├── 02_install.yaml                  # data_dir, initdb_locale, initdb_data_checksums
    │       └── 03_config.yaml                   # shared_buffers, work_mem, wal_level
    ├── roles/
    │   ├── postgres/
    │   │   ├── main.star                        # apt.package + file.content (postgresql.conf, pg_hba.conf)
    │   │   └── templates/
    │   │       ├── postgresql.conf.j2
    │   │       └── pg_hba.conf.j2
    │   ├── patroni/
    │   │   ├── main.star
    │   │   └── templates/patroni.yml.j2
    │   └── pgbouncer/
    │       ├── main.star
    │       └── templates/pgbouncer.ini.j2
    └── _lib/
        └── runr/
            ├── main.star                        # render_service(name, exec_start, restart, user)
            └── templates/service.j2             # systemd unit template

## Inventory chaining

`manifests/main.star` сливает inventory в таком порядке:

    base.yaml
      → postgresql/01_packages.yaml
      → postgresql/02_install.yaml
      → postgresql/03_config.yaml
      → production.yaml | staging.yaml (по тэгу)

Стратегия по умолчанию — `deep_map_replace_list` (из `bundle.toml
[bundle.inventory]`). Последний источник побеждает по ключам; списки
заменяются целиком.

## Composition `roles/postgres` → `_lib/runr`

Роль `postgres` импортирует `render_service` из `@lib/runr` и вызывает
её для генерации unit-файла:

    load("@lib/runr", "render_service")

    file.content(
        path = "/etc/systemd/system/postgres-runr.service",
        contents = render_service(name = "postgres", exec_start = "..."),
        ...
    )

`render_service` внутри `_lib/runr/main.star` вызывает
`template("service.j2", ...)`. Module-relative резолв находит шаблон в
`_lib/runr/templates/service.j2` — НЕ в `roles/postgres/templates/`.
Это демонстрирует isolation между ролями и lib.

## Apply через docker (smoke-test)

    make bosun-bookworm
    docker run --rm -v $(pwd):/work bosun-test-base:latest sh -c '
        cp /work/target/bookworm-release/bosun /usr/local/bin/bosun
        bosun bundle validate \
          --bundle /work/examples/multi-role-pg/bundle \
          --tags=staging
    '

Ожидаемый exit-code — 0; вывод — `evaluate OK, N resources registered`.
