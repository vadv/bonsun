# pgbouncer-cluster

Реалистичный production-like example: pgbouncer под управлением runr (либо
systemd — выбор по факту `init_system`) с cross-resource notify, валидатором
конфига и health-check'ом. Изменение `pgbouncer.ini` через `reload_on`
триггерит defer `reload:pgbouncer`; изменение unit-файла через `restart_on`
— defer `restart:pgbouncer`.

## Что делает example

1. Ставит пакет `pgbouncer`.
2. Пишет `/etc/pgbouncer/pgbouncer.ini` с pre-replace валидатором
   (`pgbouncer -V -c {new_path}`) — синтаксически невалидный INI не попадает
   на live-путь.
3. Пишет `/etc/pgbouncer/userlist.txt`.
4. Пишет INI-файл runr-юнита `/etc/runr/pgbouncer.service` (генерация —
   через `_lib/runr/render_service`).
5. Через `service.unit(state="running", reload_on=[...], restart_on=[...])`
   реально стартует сервис.
6. Health-check (`pg_isready -h 127.0.0.1 -p 6432`, 5 ретраев по 3 секунды)
   гарантирует, что после reload/restart pgbouncer реально принимает
   соединения.

## Prerequisites

- Debian-based нода под root.
- apt-кеш доступен, пакет `pgbouncer` установится из дефолтных репозиториев.
- Существует системный пользователь `postgres` (создаётся пакетом
  `postgresql-common`). Если его нет — добавьте `users.user(name="postgres", ...)`
  в роль до `file.content`.
- На ноде запущен **либо** runr daemon (HTTP API на `127.0.0.1:8010`), **либо**
  systemd. Факт `init_system` это распознаёт сам.
- При `init_system=runr` runr должен быть готов принимать `POST /api/v1/services/.../reload`.
- На порту 6432 ничего не висит до старта pgbouncer'а.
- Журнал defers `/tmp/bosun-defers/` чист.

## Команды для запуска

Валидация без обращения к системе (для CI):

    bosun bundle validate \
        --bundle ./bundle \
        --tags=production \
        --facts ./bundle/fixtures/facts-runr.json

Ожидаемый exit 0, вывод: `evaluate OK, 5 resources registered`.

Apply на ноде с runr:

    bosun apply \
        --bundle ./bundle \
        --tags=production \
        --runr-url http://127.0.0.1:8010

Apply на ноде с systemd:

    bosun apply --bundle ./bundle --tags=production

## Expected outcome

1. Установлен `pgbouncer`.
2. `/etc/pgbouncer/pgbouncer.ini` записан, owner=postgres, group=postgres,
   mode=0o640. Файл прошёл `pgbouncer -V -c` валидацию.
3. `/etc/pgbouncer/userlist.txt` записан.
4. `/etc/runr/pgbouncer.service` записан (на ноде с systemd этот файл
   тоже создаётся, но не используется service.unit'ом — см. ниже про
   systemd-mode).
5. На ноде с runr сервис стартанул через runr API; на ноде с systemd —
   через dbus к systemd1.
6. Если в этом прогоне поменялись конфиги — в `/tmp/bosun-defers/` появилась
   запись `reload:pgbouncer` или `restart:pgbouncer`. Post-replay её
   выполняет, гоняет health-check, при успехе удаляет файл.
7. Exit 0.

При успешном health-check метрика:

    bosun_defers_executed_total{result="ok"} 1
    bosun_defers_pending 0
    bosun_resources_total{outcome="changed"} 5

При неудачном health-check (например, неправильный порт в inventory):

    bosun_defers_executed_total{result="failed"} 1
    bosun_defers_pending 1

После 3 неудачных попыток запись перетекает в `.manual_clear` и
требует оператора.

## Verification

    # 1. Пакет установлен
    dpkg-query -W pgbouncer

    # 2. Конфиги на месте и валидны
    sudo -u postgres pgbouncer -V -c /etc/pgbouncer/pgbouncer.ini

    # 3. Процесс жив
    pgrep -a pgbouncer

    # 4. Принимает соединения
    pg_isready -h 127.0.0.1 -p 6432

    # 5. Журнал defers чист после успешного прогона
    bosun status

    # 6. Повторный apply — exit 0, всё unchanged
    bosun apply --bundle ./bundle --tags=production --dry-run

## Если runr недоступен

`bosun status` покажет pending defers:

    bosun status --defers-dir /tmp/bosun-defers

Пример вывода:

    ID                          STATE    ACTION  TARGET     ATTEMPTS  ENQUEUED_AT
    runr.reload:pgbouncer       pending  reload  pgbouncer  0         2026-05-19T10:15:00Z

При следующем `bosun apply` запускается pre-replay — defer выполнится
до evaluate. Если за `--defer-max-attempts` (дефолт 3) replay
проваливается, файл переименовывается в `.manual_clear`.

После восстановления runr и ручной проверки:

    bosun status --clear runr.reload:pgbouncer

## Apply на ноде с systemd

При `init_system=systemd` тот же `service.unit` диспатчит в `systemd.service`.
В демо-примере файл `/etc/runr/pgbouncer.service` всё равно пишется на
диск (он не зависит от init), но `service.unit` его не использует — реальный
unit для systemd обычно лежит в `/etc/systemd/system/pgbouncer.service`,
который ставится пакетом или прописывается отдельным `file.content`'ом.

На реальной production-ноде рекомендуется выбрать один путь: либо переписать
unit под native systemd-формат и положить в `/etc/systemd/system/`, либо
пометить роль `if facts.init_system == "runr"` (хук недоступен в текущей
версии — используйте `tags` или несколько bundle'ов).

## Структура

    bundle/
    ├── bundle.toml                              # name, version, requires_bosun, tags
    ├── main.star                                # entry: inventory merge + роль
    ├── inventory/
    │   ├── base.yaml                            # pgbouncer.{listen,databases,users}
    │   ├── production.yaml                      # pool_mode, default_pool_size, max_client_conn
    │   └── staging.yaml                         # уменьшенные лимиты
    ├── roles/pgbouncer/
    │   ├── main.star                            # apt + file.content x3 + service.unit
    │   └── templates/{pgbouncer.ini.j2, userlist.txt.j2}
    ├── _lib/runr/
    │   ├── main.star                            # render_service / render_timer / render_cgroup
    │   └── templates/{service.ini.j2, timer.ini.j2, cgroup.ini.j2}
    └── fixtures/
        ├── facts-runr.json                      # init_system=runr для bundle validate
        └── facts-systemd.json                   # init_system=systemd для bundle validate

## Cross-resource notify

Внутри роли:

    pgbouncer_config = file.content("/etc/pgbouncer/pgbouncer.ini", ...)
    pgbouncer_users  = file.content("/etc/pgbouncer/userlist.txt", ...)
    runr_unit        = file.content("/etc/runr/pgbouncer.service", ...)

    service.unit(
        name = "pgbouncer",
        reload_on  = [pgbouncer_config, pgbouncer_users],
        restart_on = [runr_unit],
        depends_on = [runr_unit],
    )

Семантика:

- Изменился `pgbouncer.ini` → `reload:pgbouncer`.
- Изменился `userlist.txt` → `reload:pgbouncer` (дедуп с первым).
- Изменился `runr/pgbouncer.service` → `restart:pgbouncer` (приоритет
  выше reload, итог — один restart).
- Изменились и тот, и другой → один `restart:pgbouncer`.

Дедуп выполняется в orchestrator'е до записи defer'а на диск.
