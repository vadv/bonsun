# pgbouncer-cluster

Реалистичный пример bundle на одной ноде: pgbouncer под управлением
runr (либо systemd — на выбор по факту `init_system`) с
cross-resource notify. Изменение `pgbouncer.ini` через
`reload_on` триггерит defer `reload:pgbouncer`, изменение
unit-файла через `restart_on` — defer `restart:pgbouncer`.

Что демонстрирует пример:

- `service.unit(name=..., state="running", reload_on=[...],
  restart_on=[...])` — абстрактный диспатчер, выбирающий runr.service
  или systemd.service по `init_system`.
- `file.content(..., validate_with=["pgbouncer", "-V", "-c",
  "{new_path}"])` — pgbouncer-валидатор синтаксиса конфига гонится
  на временной копии до replace.
- `_lib/runr/render_service(...)` — Starlark-обёртка над
  `template("service.ini.j2", ...)` для генерации INI-формата
  runr unit-файла.
- Слоистый inventory: `base.yaml` + `production.yaml` /
  `staging.yaml` через `inventory.merge(...,
  strategy="deep_map_replace_list")`.

## Запуск

Валидация (без обращения к системе, для CI bundle-репозитория):

    bosun bundle validate \
        --bundle ./bundle \
        --tags=production \
        --facts ./bundle/fixtures/facts-runr.json

Ожидаемый выход: `evaluate OK, 5 resources registered`, exit 0.

Apply на ноде с runr:

    bosun apply \
        --bundle ./bundle \
        --tags=production \
        --runr-url http://127.0.0.1:8010

Что произойдёт:

1. Установлен пакет `pgbouncer` (apt).
2. Создан `/etc/pgbouncer/pgbouncer.ini` — pgbouncer запускает
   `pgbouncer -V -c <new_path>` для синтаксис-проверки; если
   валидатор вернул не 0 — файл не записан, exit 1.
3. Создан `/etc/pgbouncer/userlist.txt`.
4. Создан `/etc/runr/pgbouncer.service` (INI runr).
5. `service.unit` устанавливает состояние running и enable=true.
6. Если конфиг или userlist изменились — в журнал defers
   попадает `reload:pgbouncer`; если поменялся unit-файл —
   `restart:pgbouncer`. Дедуп по приоритету (restart > reload).
7. После apply defers replay-фаза выполняет операцию через runr
   HTTP API, далее идёт health-check `pg_isready -h 127.0.0.1
   -p 6432` с 5 ретраями по 3 секунды.

## Apply на ноде с systemd

    bosun apply \
        --bundle ./bundle \
        --tags=production

При `init_system=systemd` тот же `service.unit` диспатчит в
`systemd.service`. Шаблоны `_lib/runr/templates/service.ini.j2`
становятся избыточными — для systemd unit-файлов используется
`/etc/systemd/system/pgbouncer.service` через стандартный
systemd-формат. В демо-примере это просто не triggered: файл
по-прежнему пишется, но `service.unit` не использует
`/etc/runr/...` на systemd-ноде. На реальном production
рекомендуется выбрать один путь (либо переписать через native
systemd unit, либо пометить роль `if facts.init_system == "runr"`).

## Если runr недоступен

`bosun status` после неудачного apply покажет pending defers:

    bosun status --defers-dir /tmp/bosun-defers

Пример вывода:

    ID                                   STATE    ACTION  TARGET     ATTEMPTS  ENQUEUED_AT
    01HZ...XYZ                           pending  reload  pgbouncer  0         2026-05-19T10:15:00Z

При следующем `bosun apply` запускается pre-replay — defers
выполнятся первыми, до evaluate. Если за 3 attempts replay
проваливается — файл переименовывается в `.manual_clear`,
дальше требует ручного `bosun status --clear=<id>`.

## Структура

    bundle/
    ├── bundle.toml                              # name, version, requires_bosun, tags
    ├── main.star                                # entry: inventory merge + роль
    ├── inventory/
    │   ├── base.yaml                            # pgbouncer.{listen,databases,users}
    │   ├── production.yaml                      # pool_mode, default_pool_size, max_client_conn
    │   └── staging.yaml                         # уменьшенные лимиты для staging
    ├── roles/
    │   └── pgbouncer/
    │       ├── main.star                        # apt, file.content x3, service.unit
    │       └── templates/
    │           ├── pgbouncer.ini.j2
    │           └── userlist.txt.j2
    ├── _lib/
    │   └── runr/
    │       ├── main.star                        # render_service / render_timer / render_cgroup
    │       └── templates/
    │           ├── service.ini.j2
    │           ├── timer.ini.j2
    │           └── cgroup.ini.j2
    └── fixtures/
        ├── facts-runr.json                      # init_system=runr для validate
        └── facts-systemd.json                   # init_system=systemd для validate

## Cross-resource notify (внутри pgbouncer-роли)

```
file.content("/etc/pgbouncer/pgbouncer.ini")   handle pgbouncer_config
file.content("/etc/pgbouncer/userlist.txt")    handle pgbouncer_users
file.content("/etc/runr/pgbouncer.service")    handle runr_unit
service.unit("pgbouncer",
    reload_on  = [pgbouncer_config, pgbouncer_users],
    restart_on = [runr_unit],
)
```

Семантика defers (см. design «Notify-связи»):

- Изменение `pgbouncer.ini` → `reload:pgbouncer`.
- Изменение `userlist.txt` → `reload:pgbouncer` (дедуп с предыдущим).
- Изменение `runr/pgbouncer.service` → `restart:pgbouncer`
  (приоритет выше reload, итог — один restart).
