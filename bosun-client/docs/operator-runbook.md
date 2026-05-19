# Operator runbook

Что делать оператору, разворачивающему bosun на парке нод. Покрывает
установку, периодический запуск, дебаг провального apply'я, recovery,
семантику defers, известные ограничения.

Этот документ — справочник под конкретные задачи. Концептуальные объяснения
вынесены в [bundle-authoring.md](bundle-authoring.md).

## Установка

### Из исходников

```sh
cargo install --path crates/bosun-cli
sudo install -m 0755 ~/.cargo/bin/bosun /usr/local/bin/bosun
bosun version
```

Для production-нод имеет смысл собирать единожды под целевую glibc
(bookworm-slim) и раскатывать бинарь:

```sh
make bosun-bookworm           # собирает в target/bookworm-release/bosun
scp target/bookworm-release/bosun node:/usr/local/bin/bosun
ssh node 'sudo install -m 0755 /usr/local/bin/bosun /usr/local/bin/bosun'
```

### Из apt-пакета

Сборка `.deb` через дистрибутивный CI лежит вне этого репозитория; для
deployment'а парка в долгосрочной перспективе используйте именно её. Пакет
должен класть:

- `/usr/local/bin/bosun` — бинарь.
- `/etc/runr/bosun.service` или `/etc/systemd/system/bosun.timer` —
  периодический запуск (см. ниже).
- `/var/lib/bosun/`, `/var/log/bosun/`, `/var/backups/bosun/` — пустые
  директории под root:root 0750.

### Каталоги, которые должны существовать

bosun создаёт их сам при апплае (`ensure_dirs`), но для production-ноды
лучше иметь их в пакете:

- `--state-dir` (`/var/lib/bosun`) — internal state, индекс бэкапов.
- `--log-dir` (`/var/log/bosun`) — логи прогонов.
- `--backup-dir` (`/var/backups/bosun`) — бэкапы файлов перед replace.
- `--defers-dir` (`/tmp/bosun-defers`) — journal defers (tmpfs).
- Директория для `--metric-file` (`/var/lib/node_exporter/textfile_collector/`) —
  под node_exporter.

## Периодический запуск

bosun — one-shot процесс: запустился, применил, завершился. Парк нужен
запуск раз в N минут. Три варианта.

### runr-таймер

Для нод под runr создайте `/etc/runr/bosun.service`:

```ini
[Service]
ExecStart = /usr/local/bin/bosun apply --bundle /etc/bosun/bundle --tags production
User      = root
Group     = root
Autostart = false
```

И `/etc/runr/bosun.timer`:

```ini
[Timer]
OnCalendar = *:0/5
Unit       = bosun.service
```

`OnCalendar=*:0/5` — каждые 5 минут. Параметр читается runr'ом
аналогично systemd. Логи доступны через runr API:
`curl http://127.0.0.1:8010/api/v1/services/bosun/logs`.

### systemd-таймер

Для нод под systemd создайте `/etc/systemd/system/bosun.service`:

```ini
[Unit]
Description = bosun apply
After       = network-online.target

[Service]
Type      = oneshot
ExecStart = /usr/local/bin/bosun apply --bundle /etc/bosun/bundle --tags production
```

И `/etc/systemd/system/bosun.timer`:

```ini
[Unit]
Description = Run bosun every 5 minutes

[Timer]
OnBootSec         = 2min
OnUnitActiveSec   = 5min
AccuracySec       = 30s
RandomizedDelaySec = 60s

[Install]
WantedBy = timers.target
```

Подхватите и включите:

```sh
sudo systemctl daemon-reload
sudo systemctl enable --now bosun.timer
```

`RandomizedDelaySec` важен для парка: без него все ноды стартуют в одну
и ту же секунду, нагружая backend'ы (apt mirror, bosun-server).

### cron

Простейший вариант для тестовых стендов — строка в `/etc/cron.d/bosun`:

```
*/5 * * * * root /usr/local/bin/bosun apply --bundle /etc/bosun/bundle --tags=production >> /var/log/bosun/cron.log 2>&1
```

Минусы по сравнению с таймером: нет randomized delay, нет логов через
journal, cron mail spam при exit != 0.

## Lock-файл и concurrent runs

bosun берёт advisory-lock `flock(/var/run/bosun.lock)`. Если другая
инстанция уже работает, новая инстанция логирует
`another bosun is running, exiting` и завершает работу с **exit 0**
(не ошибка — нормальный кейс при таймерном запуске поверх долгого apply'я).

Lock освобождается при exit'е процесса. При SIGKILL (OOM, kill -9) lock
останется flock'нутым только пока запись flock-таблицы ядра жива — после
смерти процесса flock автоматически снимается, файл остаётся на диске.

Если вы вручную запускаете `bosun apply` параллельно с таймером и видите
unexpected exit 0 — посмотрите в `journalctl` или stdout: там будет
явная диагностика.

`--lock-path` перебивает дефолт. Для отладки можно положить во временный
файл: `--lock-path /tmp/bosun-debug.lock`.

## Логи

### stdout / stderr

Бинарь пишет структурированный лог через `tracing`:

- `--log-format=text` (дефолт) — человекочитаемые строки.
- `--log-format=json` — каждая строка JSON, для коллекторов.

При запуске под systemd/runr эти строки попадают в journald (соответственно
`journalctl -u bosun.service` или runr logs API).

### --log-dir

Дополнительно агент кладёт файловые логи в `--log-dir` (`/var/log/bosun/`).
Один файл на прогон, имя содержит timestamp.

### --log-level

`info` — дефолт. `debug` нужен, когда apply упал и непонятно почему.
Заметно увеличивает объём логов; для production-таймера держите `info`.

```sh
bosun apply --bundle /etc/bosun/bundle --tags=production --log-level=debug
```

### Дебаг конкретного ресурса

Если упал один ресурс, посмотрите его trace:

```sh
journalctl -u bosun.service -n 200 --no-pager | grep -B 2 -A 5 'apt.package'
```

В JSON-логе `resource_id` всегда присутствует — фильтровать удобнее через
`jq`:

```sh
journalctl -u bosun.service -o cat | jq 'select(.resource_id == "apt.package:nginx")'
```

## Prometheus integration

`bosun.prom` пишется атомарно в `--metric-file`. Стандартный путь
`/var/lib/node_exporter/textfile_collector/bosun.prom` — node_exporter
с `--collector.textfile.directory=/var/lib/node_exporter/textfile_collector/`
подхватит.

Алерт-минимум для парка:

```yaml
# bosun не запускался дольше 15 минут
- alert: BosunStale
  expr: time() - bosun_last_attempt_timestamp_seconds > 15 * 60
  for: 5m

# Apply упал на последнем прогоне
- alert: BosunApplyFailed
  expr: bosun_last_run_exit_code != 0 and bosun_last_run_exit_code != 2
  for: 5m

# Висят manual_clear defers
- alert: BosunManualClearPending
  expr: increase(bosun_defers_executed_total{result="manual_clear"}[1h]) > 0

# Факт упорно Unknown на массиве нод
- alert: BosunFactDegraded
  expr: avg by (fact) (bosun_fact_state{fact!=""}) > 0.5
```

Алертить на `bosun_last_attempt_timestamp_seconds`, а не на
`bosun_last_run_timestamp_seconds`: первое обновляется в начале каждого
прогона, второе — только при успешном завершении. Staleness нужно ловить
именно на attempt'е.

## Дебаг провального apply'я

### 1. Посмотреть exit code

```sh
echo $?
```

`1` — частичный провал ресурса; `2` — dry-run обнаружил drift (это не
ошибка); `3` — eval (bundle сломан до apply'я); `4` — окружение (нет
прав, lock не открылся); `130` — прервали SIGTERM/deadline.

### 2. Прогнать с debug

```sh
bosun apply --bundle /etc/bosun/bundle --tags=production --log-level=debug 2>&1 | tee /tmp/bosun-debug.log
```

Логи будут шумные, но включают входной CallArgs каждого примитива,
fact-state перед plan'ом, decisions топ-сорта.

### 3. Проверить pending defers

```sh
bosun status
```

Если есть `manual_clear` — это failed-replay, разбирайтесь с тем сервисом
по таблице:

| ID | ACTION | TARGET | ATTEMPTS |
|---|---|---|---|

После разбора:

```sh
bosun status --clear <id>
# или
bosun status --clear-all-manual
```

### 4. Backup files перед replace

`file.content` перед записью копирует старый файл в `--backup-dir`. Имя
файла содержит исходный путь и timestamp. Если apply'у нужно откатить
конфиг руками:

```sh
ls -la /var/backups/bosun/ | head -5
cp /var/backups/bosun/etc-nginx-nginx.conf.2026-05-19T10-15-00 /etc/nginx/nginx.conf
```

Старые бэкапы агентом не чистятся автоматически (это не purpose) — заведите
logrotate-job или периодический cron.

### 5. `.new` файлы на диске

Если `validate_with` упал, рядом с целевым файлом останется `.new`:

```
$ ls /etc/pgbouncer/
-rw-r----- postgres postgres pgbouncer.ini       # old, оставлен нетронутым
-rw-r----- postgres postgres pgbouncer.ini.new   # новый, не прошёл валидатор
```

Можно посмотреть diff и понять, что bundle сгенерил неправильно:

```sh
diff /etc/pgbouncer/pgbouncer.ini{,.new}
```

После разбора удалите `.new` руками. Bosun сам не чистит — это
forensics-артефакт.

### 6. Сломанный bundle до apply'я

Exit 3 — значит, eval упал. Запустите `bundle validate`:

```sh
bosun bundle validate --bundle /etc/bosun/bundle --tags=production
```

Диагностика будет с указанием файла и строки Starlark.

Если bundle зависит от факта (`service.unit` смотрит `init_system`),
понадобится fixture:

```sh
bosun bundle validate --bundle /etc/bosun/bundle --tags=production \
    --facts /etc/bosun/bundle/fixtures/facts-runr.json
```

## Recovery

### Deadline истёк (exit 130)

`--deadline-sec` (дефолт 600) — глобальный лимит. По истечении агент
получает SIGTERM-семантику: текущий ресурс докатывается до точки прерывания,
defers сохраняются, прогон завершается с exit 130. Следующий apply
доделает работу.

Что проверить:

- Какой ресурс зависал. В логе `apply: resource <id> timed out`.
- Нет ли блокировок apt (`dpkg --configure -a` после crash'а).
- Нет ли висящего runr/systemd reload, ждущего timeout systemd-job.

Если 600 секунд мало — увеличьте через флаг таймера. Для медленных
apt-операций ставьте 900-1200.

### SIGTERM от runr/systemd (exit 130)

Таймер ушёл в timeout, оператор kill'нул процесс, нода ушла в reboot. То
же поведение, что у deadline. Defers сохранены (если успели записаться),
следующий apply подберёт.

### Crash mid-apply

bosun паникает только при программных ошибках (баг в Rust-коде). Контрактные
ошибки — через Result, не через panic. Если процесс упал с SIGSEGV/SIGABRT —
это баг агента, пишите issue. Apply возможно остался в полу-применённом
состоянии:

- `file.content` атомарен через `tempfile + rename`, поэтому либо весь
  файл стоит, либо стоит старый.
- `apt.package` атомарен в пределах dpkg-state (после crash'а нужен
  `dpkg --configure -a`; следующий apply сам его вызовет).
- Defers, написанные до crash'а — на диске, replay подберёт.
- `apt.update_cache` — частично работающий, скорее всего, оставит
  половинчатый `/var/cache/apt/`. Следующий apply прогонит cache
  заново (если возраст превысил `max_age_sec`).

### Reboot ноды

`/tmp/bosun-defers/` стирается. Это by design: после reboot все сервисы
рестартятся сами, накопленные reload-запросы прошлой жизни ноды
неактуальны.

Если по операционным причинам нужен persistent journal, перебивайте
`--defers-dir` на `/var/tmp/bosun-defers` — но имейте в виду, что
после reboot бесполезные reload'ы выполнятся всё равно.

## Defers semantics

Подробности и lifecycle см. в [bundle-authoring.md](bundle-authoring.md),
здесь — operator-shortcuts.

- **At-least-once.** Запись остаётся на диске, пока успешно не выполнится
  или не уйдёт в `.manual_clear`. После каждого `bosun apply` журнал
  обрабатывается дважды (pre + post).
- **Tmpfs by design.** Reboot обнуляет `/tmp/bosun-defers/`. После reboot
  managed-сервисы стартуют сами через init-систему, накопленный
  `reload:<name>` неактуален.
- **Дедуп.** В пределах одного `(action, target, init_system)`:
  `restart > reload_or_restart > reload`. Лексическая сортировка имён
  файлов даёт правильный порядок выполнения через префиксы `0r/1r/2r/3c/4d`.
- **Max attempts.** Дефолт 3 (`--defer-max-attempts`). При исчерпании
  попыток файл переименовывается в `.manual_clear`, replay его игнорирует.
- **manual_clear требует оператора.** `.manual_clear` файлы видны в
  `bosun status`, не обрабатываются replay'ем. После разбора —
  `bosun status --clear`.

Метрика `bosun_defers_executed_total{result="manual_clear"}` показывает,
сколько раз replay упёрся в max attempts за прогон. На прирост этой
метрики стоит держать алерт.

## Status quo

- **service.unit на нерегулярных init-системах не работает.** Если факт
  `init_system` = `unknown` — `service.unit` падает. Используйте
  `runr.service` / `systemd.service` напрямую или дополните facts collector.
- **Никаких rollback'ов на уровне ресурсов.** Если apply упал в середине,
  файлы, которые уже записаны, остаются. Это сознательный выбор: state
  reconciliation, а не транзакция.
- **bosun не root — не deploy-ready.** Текущая реализация требует root
  для apt, useradd, dbus к systemd, файлы под root, советские права.
  Polkit-правила для non-root не настроены.
- **Bundle distribution out of scope.** Bundle — это директория. Доставка
  на ноду (cosign, signed tar.gz, HTTPS-pull от bosun-server) — задача
  следующей итерации.
- **`bosun connect` пока живёт отдельно.** В этом крейте только
  `bosun apply`. Server-managed mode — в репозитории bosun-server.
- **Cross-bundle notify identity отсутствует.** `reload_on` / `restart_on`
  принимают только handle'ы из текущего bundle. Эскейп-хэтч
  «дёрни сервис по имени» — следующая итерация.
- **Validator argument templating ограничен.** Поддерживается только
  `{new_path}`. Поддержка `{path}`, `{owner}`, `{group}` — позже.
- **Defers durability при NTP-сдвиге.** Если системные часы прыгнули
  назад на несколько часов после записи defer'а, replay будет считать
  его future-dated. Не критично — следующий нормальный прогон выполнит.
