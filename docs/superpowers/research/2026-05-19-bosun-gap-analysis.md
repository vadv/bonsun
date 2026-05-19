# bosun gap-анализ: что не завезли из chiit/postgres-chiit
Дата: 2026-05-19
Автор: research-агент по заданию dmitrivasilyev

## TL;DR

bosun-client MVP покрывает только **apt.package**, **file.content** и **template()**. Phase D-L уже планируют service-примитивы (`runr.service`, `systemd.service`, `service.unit`), `command.run`, validate_with, health-check, deferred journal — это закроет большую часть Notify-цепочек chiit. После Phase L останутся следующие **критические** gap'ы, без которых нельзя катить bosun на парк PG:

1. **Vault/secrets бутстрап и chiit-server proxy.** postgres-chiit получает 100% секретов и inventory через `lib/vars/secret.go` + `lib/vars/manager.go`, ходящие в `chiit-server` (HTTP с warden-discovery). bosun имеет только in-process `SensitiveStore` без bootstrap-механизма. Без этого ноду не подготовить — пароли postgres/replication/monitor/owner_* и сертификаты должны где-то жить.
2. **Bundle distribution + p2p re-exec.** chiit умеет self-upgrade через peer-to-peer (`lib/p2p/server.go`, `cmd/upgrade.go`) и SIGHUP-самостоп при обновлении бинаря. bosun дизайн упоминает push-rollout (out-of-scope), но в крейте этого нет — а без обновляемого агента на 40-60k нод не управиться.
3. **Discovery-факты для PG-парка.** chiit вызывает `pg.MustGetIsMaster`, `pg.ListUsers` напрямую внутри роли через SQL по unix-сокету (`lib/utils/pg/`). bosun-facts имеет 6 базовых фактов (hostname, cpu_count, memory_mb, init_system, is_pod, installed_packages) — ни одного PG-specific. Discovery-таксономия в дизайне есть, имплементации нет.
4. **users.account / users.group.** chiit создаёт системных пользователей через `adduser`/`groupadd` (`chiit/lib/providers/users/`). postgres-chiit ставит `postgres:5432`. bosun не имеет аналога — а без user/group нет ни одного PG-кластера.
5. **directory.tree.** Каждая роль chiit начинается с десятков `files.CreateDirectory()`. bosun имеет только `file.content` (создаёт parent через `mkdir -p`, но без recursive owner/group/permission на промежуточные сегменты).
6. **package.repository / apt-key.** chiit имеет `apt.AddRepo`, `apt.AddKeyURL`, `apt.SetRootRepo` — без этого нельзя добавить ozon-репозиторий, без которого не ставится PG/Patroni/wal-g.
7. **patroni-guard sync.** `patroni_functions.EnsurePatroniConfigSynced` — обязательный шаг перед стартом patroni: ждёт 200 от `http://127.0.0.1:8009/patroni-update-config`. Эта точка синхронизации в дизайне bosun не отражена.
8. **pacer.** chiit размазывает CPU-нагрузку 30 секундами `pacer.Tick()` (60-100ms между шагами). На 40-60k нод одновременный шторм apt-get'ов кладёт зеркало. bosun не имеет throttling между ресурсами.
9. **TemplateWithValidation равно по поведению chiit** — это покрывается Phase H (`validate_with=`), но дизайн принимает только `{new_path}`, тогда как chiit передаёт реальный текст конфига и при ошибке оставляет `.new` файл для forensics. Совпадает в дизайне, проверить в имплементации.

## Категория A. Примитивы / providers

### A.1 Уже в bosun
- `apt.package` — есть. Bosun ставит `name=version`, поддерживает `--allow-downgrades` и `--allow-change-held-packages`. Покрывает `chiit/lib/providers/packages/apt/`. **Однако** не покрывает `Upgrade`-семантику (без указания версии) и `Remove/Purge` — в chiit это `apt.Upgrade`/`apt.Remove`.
- `file.content` — есть. Поддерживает atomic write, mode/owner/group, sha256 dedup. Покрывает `chiit/lib/providers/file/templater/templater.go` без `validate_with` (это Phase H).
- `template()` (через minijinja Strict) — есть. Покрывает Go template из `postgres-chiit/lib/template_functions/main.go`.

### A.2 В плане Phase D-L (учитывать)
- `runr.service` / `runr.timer` / `runr.cgroup` — Phase D
- `systemd.service` / `systemd.timer` — Phase E
- `service.unit` (абстрактный dispatcher) — Phase F
- `command.run` (с `deferred=True`, `only_if=`, `not_if=`) — Phase G
- `validate_with=` на file.content/file.template/service.unit — Phase H
- `health_check_cmd` / `health_check_url` — Phase I
- `bosun status`, defers pruning, CLI команды — Phase J
- BDD интеграция + Docker — Phase K
- `_lib/runr/main.star` с `render_service/render_timer/render_cgroup` — Phase L

### A.3 Чего нет ни в bosun, ни в плане Phase D-L (критично)

#### A.3.1 `users.user` / `users.group` — критично
**Где в chiit:** `chiit/lib/providers/users/user.go`, `group.go`.
Принимают `User{Name, UID, Group, Shell, NoCreateHome, Home}` и `Group{Name, System, GID}`. Реализация — `os/user.Lookup` для идемпотентности + shell-команда `adduser`/`groupadd` (chiit пишет shell, а не direct syscall).
**Зачем:** postgres-chiit в `roles/prepare/main.go:51-63` ставит `postgres:5432/postgres:5432`. Без этого никакая PG-конфигурация не возможна.
**Семантика:** идемпотентная проверка через `user.Lookup`, при отсутствии — adduser.
**Приоритет:** **критично**.

#### A.3.2 `directory.tree` — критично
**Где в chiit:** `chiit/lib/providers/file/filemanager/filemanager.go:25-52` (Directory) и postgres-chiit/lib/files/file.go:22-24 (`CreateDirectory`).
Семантика: создать директорию рекурсивно (`os.MkdirAll`), поставить mode, поставить chown через `setOwner` (lookup uid/gid). Идемпотентно: если уже есть — поправить permissions/owner.
**Зачем:** каждая роль начинается с 5-10 `files.CreateDirectory()` (см. `roles/postgres/main.go:42-83`, `roles/patroni/install.go:28-34`).
**Приоритет:** **критично**.

#### A.3.3 `file.symlink` — критично
**Где в chiit:** `chiit/lib/providers/file/filemanager/link.go`.
`Link(src, dst)` создаёт симлинк, идемпотентно: если симлинк уже указывает на src — no-op; если на другое — удаляет и пересоздаёт; если файл/директория — удаляет и пересоздаёт.
**Зачем:** `roles/postgres/install_nix.go:32-36` создаёт ~50 симлинков для PG bin/lib/share (`/etc/nix-paths/...` -> `/usr/lib/postgresql/...`). `roles/patroni/install.go:24-25` симлинкает `/usr/bin/patroni-${ver}` -> `/usr/local/bin/patroni`. `roles/runr/chiit.go:64-67` симлинкает systemctl/journalctl -> runr в runr-init mode.
**Приоритет:** **критично**.

#### A.3.4 `file.delete` — критично
**Где в chiit:** `chiit/lib/providers/file/filemanager/filemanager.go:53-110`.
Рекурсивно удаляет файл или директорию. Notify-changed если что-то было удалено.
**Зачем:** repos cleanup (`roles/repos/repos.go:86-94` — удаление устаревших repo-list файлов), `roles/journald/config.go:30` — `files.Delete(/etc/systemd/journald.conf.d/journald.conf)`. Также Patroni guard трейггер-файл (`roles/postgres_manage/init_config.go:54`).
**Приоритет:** **критично** для миграции. bosun MVP design explicitly «нет `Diff::Remove`».

#### A.3.5 `apt.repository` / `apt.key` / `apt.root_repo` — критично
**Где в chiit:** `postgres-chiit/lib/apt/apt.go:40-122`.
- `AddKeyURL(url)` — скачать ключ во временную директорию с TTL-кэшем 5 минут, выполнить `apt-key add`. Под retry в течение 5 минут с recover-from-panic.
- `AddRepo(name, content)` — записать в `/etc/apt/sources.list.d/<name>.list`.
- `SetRootRepo(content)` — переписать `/etc/apt/sources.list`.
- `RemoveRepo(name)` — удалить файл.

**Зачем:** `roles/repos/repos.go:22-63` подключает ozon-репозиторий, postgresql.org, timescaledb, pg_probackup. Без них postgresql даже не скачать.
**Приоритет:** **критично** (нет PG-репозитория — нет PG).

#### A.3.6 `apt.update` (явный) — критично
**Где в chiit:** `postgres-chiit/lib/apt/apt.go:164-168` и `roles/repos/update.go`.
Семантика: если `/var/cache/apt/pkgcache.bin` старше 1 часа — `apt-get update` под lock-file. После repo-list изменения — принудительный update.
**Зачем:** ленивый update иначе свежий пакет не виден. В bosun apt.package делает lazy update только при невозможности зарезолвить, но `apt.repository` без принудительного update просто не виден.
**Приоритет:** **критично**.

#### A.3.7 `apt.upgrade` (без указания версии) — важно
**Где в chiit:** `postgres-chiit/lib/apt/apt.go:144-161`.
Семантика: установить последнюю candidate-версию, если она новее installed. Отличается от `apt.Install` (которая ставит точную версию или ничего не делает).
**Зачем:** `roles/repos/risk.go:51-105` — пакеты безопасности (openssh-server, sudo, locales и т.д.) под `Upgrade`. Без него нельзя докатить security-fixes.
**Приоритет:** **важно**.

#### A.3.8 `apt.purge` / `apt.remove` — важно
**Где в chiit:** `postgres-chiit/lib/apt/apt.go:74-82`.
Семантика: `apt-get purge -qy <name>`.
**Зачем:** `roles/repos/repos.go:65-86` удаляет update-notifier-common, ubuntu-advantage-tools, snapd, command-not-found, ubuntu-pro-client из jammy-нод.
**Приоритет:** **важно**.

#### A.3.9 `apt.install_with_policy_rc_d_101` — низкий, специфично для PG-upgrade
**Где в chiit:** `postgres-chiit/lib/apt/apt.go:170-215` (`InstallPolicyRC101`).
Семантика: перед apt-install подкладывает `/usr/sbin/policy-rc.d` с `exit 101`, чтобы apt не стартовал сервис. Используется для major-upgrade PG (PG10->PG14): пакеты поставить, но postgresql.service не дёргать.
**Зачем:** PG version upgrade.
**Приоритет:** низкий (нужен только при major-upgrade flow).

#### A.3.10 `file.download` — важно
**Где в chiit:** `chiit/lib/providers/file/downloader/download.go`.
Семантика: HTTP GET через `http.Client` с TLS InsecureSkipVerify, atomic write через `.tmp` + rename, опциональная MD5-проверка, кеширование (если есть и checksum совпадает — не качаем).
**Зачем:** apt-key добавляет через download. Также может пригодиться для wal-g/pg_probackup и других OOB tarball'ов.
**Приоритет:** **важно**.

#### A.3.11 `wait.port` — важно
**Где в chiit:** `chiit/lib/providers/wait/port.go`.
Семантика: `net.DialTimeout("tcp", addr, 100ms)` под retry timeout, при недоступности порта — wait.
**Зачем:** `roles/patroni/start.go:33,39` — `wait.Port(patroni.restapi.listen, 30s)` после старта patroni. Без него race между EnableAndStart и dependent SQL-операциями.
**Приоритет:** **важно** (нужно для PG-инициализации).

#### A.3.12 `cron.entry` / `cron.d` — важно
**Где в chiit:** не отдельный примитив, а через `file.content` пишет в `/etc/cron.hourly/logrotate` и `/etc/logrotate.d/hourly/*` (`roles/logrotate/main.go:23-34`). 
**Зачем:** в чисто systemd/runr-мире, по-видимому, cron-entries не нужны — `roles/logrotate/main.go:35-43` уже в runr-режиме делает `runr.timer` с `OnCalendar=hourly`. Но bionic-ноды (без runr) идут через cron.
**Приоритет:** **важно**, если есть bionic-ноды. Если все ноды jammy+runr — это `runr.timer` (уже в Phase D).

#### A.3.13 `sysctl.value` — средне
**Где в chiit:** `postgres-chiit/roles/sysctl/main.go`.
Семантика: `file.content` в `/etc/sysctl.d/60-chiit.conf` + Notify-only `sysctl -p <file>`. Это композитный, не отдельный примитив.
**Зачем:** PG tuning (vm.swappiness, kernel.sched_*, net.core.rmem_max и т.д.).
**Приоритет:** **средне** — закрывается через `file.content` + `command.run(deferred)`, но идеально бы иметь именованный примитив для UX.

#### A.3.14 `etc_hosts.entry` — низко, локализованно
**Где в chiit:** `postgres-chiit/roles/hosts/main.go`.
Семантика: добавить `127.0.0.1\tlocalhost` если ещё нет. Регекспом ищет.
**Зачем:** staging-фикс для k8s-нод.
**Приоритет:** **низко** — закрывается через `file.content` с шаблоном или регексп-инжектом.

#### A.3.15 `kernel.module` / `modprobe.d` — нет
**Где в chiit:** не используется. PG-парк не модифицирует ядро.
**Приоритет:** **не нужно** для замены chiit.

#### A.3.16 `mount.point` / fstab — нет
**Где в chiit:** не используется. На k8s mount управляет k8s, на KVM — провижионер.
**Приоритет:** **не нужно**.

#### A.3.17 `iptables` / `ufw` — нет
**Где в chiit:** не используется (только `ipset` ставится для chaosd).
**Приоритет:** **не нужно**.

#### A.3.18 `ssh.authorized_keys` / `ssh.known_hosts` — нет
**Где в chiit:** не используется.
**Приоритет:** **не нужно**.

#### A.3.19 `cert.tls` / certificate management — критично
**Где в chiit:** `postgres-chiit/lib/cert_manager/models.go` + `lib/vars/certificate.go`. Сертификаты приходят с chiit-server через `GetCertByID`, кэшируются в `manager.certCache`. CertificateData = {Cert, CaBundle, PrivateKey}. Каждый файл затем кладётся через `files.Template`.
**Также:** `roles/postgres/init_ssl.go` генерирует self-signed openssl сертификат для postgres через shell-команду.
**Зачем:** patroni-cluster ssl, postgresql server_cert/server_key, pgbouncer, копи-кэт. Каждый кластер имеет свой сертификат.
**Приоритет:** **критично**.
**Сейчас в bosun:** SensitiveStore есть, но нет провайдера для cert.tls который умеет распаковать pem/key + правильные права + reload notify.

#### A.3.20 `archive.extract` / `tar.unpack` — нет
**Где в chiit:** не используется (всё через apt).
**Приоритет:** **не нужно**.

#### A.3.21 `git.checkout` — нет
**Где в chiit:** не используется.
**Приоритет:** **не нужно**.

#### A.3.22 `cgroup.v2` (низкоуровневое управление кроме `runr.cgroup`) — критично для k8s
**Где в chiit:** `roles/pod/io.go` записывает в `/sys/fs/cgroup/.../io.max` напрямую. `roles/prepare/infra_cgroup.go` создаёт `/sys/fs/cgroup/infra/` (KVM). `roles/prepare/main.go:128-150` создаёт runr.cgroup (k8s).
**Зачем:** I/O лимиты для k8s-нод (zero-trust I/O QoS). Тоже что `cgroup.io_max` запись.
**Приоритет:** **критично** для k8s-нод. `runr.cgroup` (Phase D) частично закрывает; ручная запись в `/sys/fs/cgroup` — open question.

#### A.3.23 `pg_sql.query` / `pg_sql.exec` — критично
**Где в chiit:** `chiit/lib/utils/pg/` — CreateRole, CreateDatabase, GrantRole, GrantSelect, GrantExecute, SetRoleSetting, SetConnectionLimit, InstallExt, GrantUsageOnSchema. Через `database/sql` + `lib/pq`, идемпотентные SQL-операции.
**Зачем:** `roles/postgres_manage/users.go`, `roles/postgres_manage/extensions.go`, `roles/postgres_manage/probackup_users.go` — без них нет ни одного пользователя в PG.
**Приоритет:** **критично**. Это не factor-out в "factal-факты", это write-операции. Phase G имеет `command.run` (можно затащить через `psql -c`), но идиоматичнее иметь нативный примитив.

#### A.3.24 `dpkg.selections` (held) — низкий
**Где в chiit:** не используется в постоянной работе, но `apt.package` имеет `allow_change_held=true`, который обходит ручной hold.
**Приоритет:** **низко**.

## Категория B. Секреты и crypto

### B.1 Текущая модель chiit

`postgres-chiit/lib/vars/secret.go` + `lib/vars/manager.go`:
- Vault path формируется как `infra/postgresql/<dbname>/<env>:<key>` (новый путь) или `infra/postgresql/_common/<env>_<user>_password:value` (legacy путь).
- `getVaultPath()` пробует новый, если нет — legacy. Cached через `sync.Once`.
- Все секреты прокачиваются через chiit-server (HTTP), а не напрямую к Vault. server.GetVault() даёт authenticated request с JWT.
- Локальный режим (`local-mode=true`) — прямой Vault через VAULT_ADDR.
- Retry: `getWithRetry` крутит 100ms полл-цикл пока не ответит или не upgrade'нется до `ErrVaultNotFound` (404).
- Кеш `manager.secretCache: map[string]string` живёт на весь run.
- Сертификаты: `GetCertByID(int)` — тоже через chiit-server (`/api/v1/certificates/<id>?type=certificate`).

### B.2 Текущая модель bosun

- `bosun-core::SensitiveStore` — in-process store с прятанием в логах. **Без bootstrap-механизма.** Подаётся в Starlark через `ApplyCtx.sensitive`.
- В дизайне bosun-server упомянут «bootstrap через shared secret», но это out-of-scope ресерча.

### B.3 Что критично надо завезти
1. **HTTP-клиент к chiit-server (или bosun-server) с warden discovery.** Без этого — нельзя получить ни секреты, ни inventory с control plane.
2. **Vault-path схема + retry semantics.** Совместимая со старым/новым форматом. Локальный mode на этапе bootstrap'а.
3. **TLS-сертификат resource.** `cert.tls(id=42)` который тащит cert/key/ca bundle с server'а, кладёт на диск (3 файла: cert, key, ca_bundle) с разными правами + notify соседних file.content при обновлении.
4. **Caching.** Per-run-кеш секретов чтобы не дёргать сервер per-resource.

### B.4 Bootstrap-секреты
**Сейчас:** chiit-client получает свой первый bootstrap из warden URL зашитого в бинарь (`defaultWardenDiscoveryURL`), оттуда находит chiit-server, оттуда — всё остальное. Bootstrap-токен для chiit-server (если есть) — внутри binary как const.
**Что нужно:** в bosun дизайне server↔client — bootstrap через "shared secret" (в спеке не детализировано). Это open question.

### B.5 Encryption at-rest
**Сейчас:** chiit не шифрует ничего локально. `/etc/chiit/inventory.yaml` пишется plain (0700, owner root).
**Приоритет:** **не нужно** (UNIX permissions достаточно для PG-парка).

### B.6 Secret rotation
**Сейчас:** chiit при каждом run перечитывает Vault через `GetSecret`. Rotation: chiit-server обновляет Vault, на следующем reconcile chiit-client забирает новое. `SetRoleSetting` через SQL это новое и применяет.
**Приоритет:** **важно** — для PG паролей. Полагается на reconcile-цикл.

## Категория C. Inventory / facts

### C.1 Факты, которые есть в chiit (используются как факты, хотя называются template_functions)
- `MustGetHostname()` — `hostname -f` или из `/etc/pod.conf:POD_FQDN`. **bosun: есть.**
- `MustGetHostGroup()` — первый сегмент hostname до `-`. **bosun: нет** (но легко выводится).
- `GetTotalMemMb()` / `GetTotalMemBytes()` — из `/proc/meminfo` или `memory.max` cgroup. **bosun: есть** (cgroup-aware).
- `GetCPU()` — `runtime.NumCPU()` или из `cpu.max` cgroup. **bosun: есть** (cgroup-aware).
- `GetOutboundIP()` — Dial("udp", "8.8.8.8:80") trick. **bosun: нет.**
- `IsPOD()` — `os.Stat("/etc/pod.conf")`. **bosun: есть.**
- `IsBurstablePOD()` — pod_qos != Guaranteed. **bosun: нет.**
- `IsRunRInit()` — `POD_INIT=runr` в pod.conf. **bosun: запланировано в Phase D extension `init_system::runr`.**
- `IsUbuntuBionic()` — `/etc/lsb-release` DISTRIB_CODENAME. **bosun: нет.**
- `GetAffinityList(skip, count)` — генерирует cpuset string. **bosun: нет** (но это compute fn, не factal-факт).
- `HashMod(mod, str)` — sha256 % mod. **bosun: нет** (compute fn).

### C.2 Discovery-факты (PG-specific)
- `pg.MustGetIsMaster(ctx, dsn)` — `SELECT pg_is_in_recovery()`. **bosun: нет.** Критичный (определяет, выполнять ли write-операции).
- `pg.ListUsers(ctx, dsn, inv)` — все пользователи с MD5/SCRAM хешами для odyssey. **bosun: нет.**
- `pg.ListOzonUsers(ctx, dsn)` — пользователи в группе `ozon-users`. **bosun: нет.**
- `pgServerKeyExists` / `pgServerCertExists` — `os.Stat`. **bosun: нет.**
- `pgIsInitialized` / `pgIsInRecovery` — readStatus(). **bosun: нет.**

### C.3 Inventory layout
**chiit:** `postgres-chiit/inventory/` — embed.FS, иерархически смерженный через `vars.New()`: main.yaml + production.yaml + postgresql/*.yaml + pg_mediator/*.yaml + pooler/*.yaml + (storage-inventory из chiit-server) + (local-inventory override). Merge стратегия — через `mergo.Merge` (deep map merge). `Get(key)` через `gjson.Result`.

**bosun:** `Bundle::load_dir` + `inventory.load()` + `inventory.merge()` в Starlark. Merge стратегии: `MergeStrategy::ReplaceList` / `DeepMapReplaceList` (закреплены в дизайне). Это **уже более явная модель** — отдельный gap не нужен.

### C.4 Storage-inventory (external source)
**chiit:** `chiit/lib/vars/storage_inventory/` — HTTP к chiit-server, ответ — JSON merge'ится в `m.storage`. Это per-cluster overrides (привязка cluster_name → etcd, конкретные параметры).
**bosun:** не реализовано. Out-of-scope research — но **критично** для парка.

### C.5 Canary values
**chiit:** `postgres-chiit/lib/canary/parse.go` + `lib/vars/canary.go` — отдельный YAML `inventory/canary.yaml` со структурой `production/staging × low/medium/high × <key> × value`. Severity класс ноды определяется через `hash(cluster_name) % 100` + severity-файл.
**bosun:** дизайн содержит "severity classes" в bosun-server overview, но client-side handling не реализован.
**Приоритет:** **критично** — без canary нельзя катить версии безопасно.

## Категория D. Notify / depends / order

### D.1 Notify chain
**chiit:** `handlers.Notify{Changed bool, Error error}` + `.NotifyChain(f)` (если Changed) + `.Notify(f)` (если Changed, без возврата) + `.CheckError()` (panic при Error).
**bosun:** через `Resource.reload_on` + `Resource.restart_on` + `Resource.depends_on`. Это **более структурный** подход. **gap нет.**

### D.2 only_if / not_if
**chiit:** `command.Shell{OnlyIF, NotIF}` — выполняет команду, exit-code 0 = true.
**bosun:** запланировано в Phase G `command.run(only_if=, not_if=)`.

### D.3 watch на изменение mtime файла
**chiit:** не используется. Все триггеры — Notify-цепочка.
**bosun:** не нужно.

### D.4 Self-reload chiit
**chiit:** `roles/chiit/main.go:67-89` — при изменении postgres-chiit бинаря выставляет `HUPReceived=true`, в финальном defer `cmd/run.go:124` делает `os.Exit(101)` — systemd перезапускает агента. На k8s — через runr.
**bosun:** **gap.** Когда обновляем сам `bosun` binary — что происходит? Self-exit + restart — дизайн не освещает.
**Приоритет:** **критично** на 40-60k нод (без self-upgrade нет миграции).

## Категория E. Логирование, метрики, observability

### E.1 chiit
- **Логи:** logrus с TextFormatter (ForceColors), уровни configurable через --log-level. Дополнительно `journalhook` (--enable-journald) и `lfshook` для `/var/log/chiit-<checksum>.log` (JSON формат, локальный файл для последующей загрузки на chiit-server через `m.Report(ctx, reportFile, successful)`).
- **Метрики:** Prometheus textfile в `--prometheus-metric-file`. Метрики:
  - `chiit_failures{version, severity}` — 0/1
  - `chiit_running_time{version, severity}` — float seconds
  - `chiit_last_run{version, severity}` — unix timestamp
  - `chiit_pooler{pool_mode, type, version, severity}` — info
  - `runr_version{version}`, `runr_init_version{version}` etc.
  - `pgbouncer_cron{pool_mode}` — info
- **Tracing:** нет (только logrus structured fields).

### E.2 bosun
- **Логи:** `tracing` crate (есть), text/json format, NO_COLOR. **Без journalhook.**
- **Метрики:** Prometheus textfile (есть). Тэги: `bosun_failures`, `bosun_duration_sec`, `bosun_last_run`, `bosun_resources_changed/unchanged/failed/deferred`, `bosun_fact_state{name, state_code}`. Дополнительно (по Phase D дизайну): `bosun_defers_pending`, `bosun_defers_executed_total`, `bosun_runr_reachable`, `bosun_systemd_reachable`.
- **Tracing:** структурно есть, в spans есть.

### E.3 gap
- **journald output** — chiit пишет в journald через `coreos/go-systemd/journal`. bosun этого нет. Не критично, но для k8s pod-режима полезно (journalctl собирает).
- **Загрузка report-логов на сервер.** chiit делает `m.Report(ctx, reportFile, successful)` — bosun нет. Это требует server-side endpoint. **Out-of-scope без server'а.**
- **Health endpoint /metrics в самом бинаре.** chiit нет (только textfile). bosun тоже нет. Не нужно.

### E.4 Tracing spans
bosun имеет тонкую тректировку (span на defer, span на service_action, span на validate, span на health_check). chiit — flat logrus fields. **bosun лучше**, gap нет.

## Категория F. Server interaction

### F.1 Bootstrap protocol
**chiit:** `--warden-chiit-server` URL зашит в бинарь, оттуда warden discovery возвращает список chiit-server pod'ов, выбирается hash-based (`GetHashedAddress(hostname)`). На fallback — `--warden-chiit-server-recovery`. Кэшируется в `/tmp/chiit-server-cache/<hash>.json`.
**bosun:** в дизайне есть, но не имплементировано. **gap критично** для production.

### F.2 Reconcile cycle
**chiit:** 30s через systemd timer на ноде. bosun дизайн — push-rollout с сервера.
**Сегодняшний реальный bosun:** запускается через CLI, нет цикла. Это **acceptable** для MVP-разработки, но для production либо timer (chiit-style), либо server-push (bosun-design) — обязательны. **gap.**

### F.3 Audit log
**chiit:** uploads `/var/log/chiit-<checksum>.log` (JSON-формат, все DEBUG/WARN/INFO/ERROR строки) после run через `m.Report(ctx, ...)`.
**bosun:** не упомянуто. **gap.**

### F.4 Heartbeat
**chiit:** не имеет отдельного heartbeat (только sleep-persist 15-30s после run).
**bosun:** не нужно.

### F.5 Bundle distribution
**chiit:** через apt-пакет `postgres-chiit`. Roles + inventory зашиты в бинарь через `go:embed`. **chiit и есть bundle.**
**bosun:** Bundle = директория, файл на диске. Distribution: out-of-scope research (S3/OCI в дизайне). **Критично** — нет bundle, нет конфигурации.

### F.6 mTLS / TLS
**chiit:** TLS с InsecureSkipVerify в warden и `http://` к chiit-server (через proxy). Внутри k8s — service-to-service unencrypted.
**bosun:** дизайн упоминает mTLS — не реализовано.

## Категория G. Patroni / PG-специфичное

### G.1 patroni-guard sync
**Где в chiit:** `postgres-chiit/lib/patroni_functions/patroni_guard.go:26-60`.
Перед стартом patroni — обязательный HTTP GET `http://127.0.0.1:8009/patroni-update-config`. Ждать 200 (retry 1s интервал, 1 минута timeout). Без него patroni может стартовать с устаревшей конфигурацией → split-brain.
**Приоритет:** **критично** для PG. **gap** — bosun ничего об этом не знает.

### G.2 PG version pinning
**Где в chiit:** `apt.Install(postgresql-N-nix, version)` + `inventory.GetPgVersion()`. Major upgrade — отдельный режим `--pg-version-upgrade`.
**Приоритет:** **важно** — закрыто через `apt.package` с явной версией.

### G.3 PG extensions
**Где в chiit:** `roles/postgres_manage/extensions.go` — `pg.InstallExt(dsn, "pg_stat_statements")` через SQL. Также `repairPgStorePlans` — особый случай fix.
**Приоритет:** **критично** — `pg_sql.exec` примитив нужен.

### G.4 Patroni & pgbouncer config templates
**Где в chiit:** в роли через `files.Template`. **bosun: закрыто** Phase H + Phase L.

### G.5 wal-g / pg_mediator backups
**Где в chiit:** `roles/pg_mediator/` — apt install wal-g, pg_mediator + systemd.timer + env-файлы. **bosun: закрыто** через `apt.package` + `systemd.timer` (Phase E) + `file.content`.

### G.6 PG init/data directory
**Где в chiit:** `roles/postgres/main.go:42-83` — create `/data/postgresql/`, chown postgres:postgres. **bosun: будет закрыто** через `directory.tree` (gap A.3.2).

### G.7 PG cluster status (master/replica detection)
**Где в chiit:** `pg.MustGetIsMaster` — устаревший factory-fn панически проверяет recovery. **bosun: нет** — нужен discovery-факт `pg_is_master` с failure-mode Unknown.

## Категория H. Failure modes / retry policy

### H.1 chiit failure model
- При панике в любом провайдере — main `defer recover()` (`cmd/run.go:94-103`) ловит, пишет prometheus failure metric `chiit_failures=1`, продолжает defers, exit 1.
- Большинство провайдеров sing panic при ошибке (`CheckError().panic`). Идея: ошибка — сигнал, что нода в кривом состоянии, надо звать оператора.
- DPKG lock — recover-from-panic + retry с экспоненциальной задержкой (`retry.Every`).
- Vault unavailable — `getWithRetry` с 1 минутой timeout полл-цикл 100ms.

### H.2 bosun failure model
- `Result<T, PrimitiveError>` (есть). Никакого panic в production-path.
- `PrimitiveError::is_deferrable()` — DpkgLocked / RunrUnavailable / SystemdUnavailable / Cancelled → defer next cycle. Все остальные → exit с partial-failure (continue_on_error=true) или abort (false).
- DPKG lock — есть recovery (Phase D ApplyOpts).

### H.3 gap
- **Vault retry политики** — нет в bosun (нет VaultClient).
- **Самовосстановление от corrupt state.** chiit делает `dpkg --configure -a` если видит "dpkg was interrupted" в выводе apt-get. **bosun: есть.**
- **etcd unavailable** (для patroni) — chiit не обрабатывает (это вне provider'ов; patroni сам решает). **bosun: не нужно.**
- **partial-failure rollback** — у chiit нет (выполненные изменения не откатываются). **bosun: дизайн фиксирует "при failed reload/restart bosun-side rollback не делает".** Совпадает.

## Категория I. CLI tooling

### I.1 chiit CLI
- `postgres-chiit run` (alias `r`) — основной entry point. Флаги: `--why-run`, `--local-mode`, `--hostname`, `--warden-chiit-server`, `--vault-addr`, `--storage-inventory-cluster-type`, `--storage-inventory-environment`, `--log-level`, `--checksum`, `--patch-inventory` (JSON inline patch), `--local-inventory` (YAML override), `--severity-file`, `--canary-hash`, `--prometheus-metric-file`, `--sleep-persist`, `--sleep-random`, `--timeout`, `--force`, `--backup-enable`, `--backup-dir`, `--backup-count`, `--exit-code-changes`, `--enable-journald`, `--pg-version-upgrade`.
- `postgres-chiit upgrade` (alias `u`) — self-upgrade через chiit-server (severity-aware canary).
- `postgres-chiit self-tests` — built-in BDD-like sanity checks.

### I.2 bosun CLI
- `bosun apply --bundle=PATH [--tags=,] [--dry-run] [--continue-on-error] [--log-level] [--log-format] [--format] [--no-color] [--lock-path] [--deadline-sec] [--state-dir] [--log-dir] [--backup-dir] [--metric-file]`
- `bosun bundle validate --bundle --tags --facts`
- `bosun version`
- Phase D plan: `bosun status [--defers-dir] [--format] [--clear=]`
- Phase J plan: `bosun status` — pending defers, etc.

### I.3 gap
- **`bosun self-tests`** — нет аналога. Запуск проверок в production-режиме для smoke-test.
- **`bosun upgrade`** — нет. Self-upgrade self-distribution на 40-60k нод — критично.
- **`--why-run` (dry-run)** — у bosun это `--dry-run`. **gap нет.**
- **`--local-mode`** — у bosun bundle всегда локальный (директория). **gap нет.**
- **`--patch-inventory`** (inline JSON patch) — нет. Полезно для testing-overrides.
- **`--severity-file` / `--canary-hash`** — нет. Нужно для severity-aware rollout.
- **`--exit-code-changes`** — нет. Удобно для CI.
- **`--prometheus-metric-file`** — есть (как `--metric-file`).

### I.4 Output format
- chiit: human-readable logrus output + JSON в reportFile. **bosun: Text/JSON.** **gap нет.**
- chiit: progress bar (только для upgrade). **bosun: нет** — не критично.
- chiit: summary line — нет (только полный лог). **bosun: есть** ("Summary: N add, M update, ...").

## Категория J. Testing infra

### J.1 chiit
- `chiit/Makefile`: `docker-test` — собирает `tests/Dockerfile`, прогоняет `bin/test-apt`, `bin/test-users`. Live в Docker, без моков.
- `postgres-chiit/Makefile`: `docker-test` — нет аналога per-component. Есть `make test-build` для k8s_image (test образ).
- BDD-like: `lib/defers/defers_test.go` — unit-тесты на real filesystem.
- `lib/canary/canary_test.go`, `lib/hba/hba_test.go`, `lib/pacer/pacer_test.go`, `lib/runr/ini_serialize_test.go`, `lib/runr/systemd_convert_test.go`, `lib/versions/checker_test.go`, `lib/template_functions/function_test.go`, `lib/warden/warden_test.go`, `lib/patroni_functions/archival_election_test.go`, `lib/patroni_functions/patroni_guard_test.go`.

### J.2 bosun
- Unit-tests per crate (`#[cfg(test)]`).
- BDD через `cucumber-rs` в Docker (Phase D-K плановое расширение).
- `bosun-client/docker/test-base.Dockerfile` для BDD.

### J.3 gap
- BDD scenarios для всех новых примитивов (Phase K).
- Integration-test с real chiit-server / bosun-server — нет ни у chiit (нет такого), ни у bosun. Open question.

## Категория K. Operator UX

### K.1 chiit
- Логирование: logrus с уровнями. Лог-файл в `/var/log/chiit-<checksum>.log`.
- Reporting: после run отправляет лог на chiit-server, видно в UI.
- Diff output: каждый провайдер сам печатает diff (templater печатает unified diff через `diff.Files`).
- Backup: при изменении файла — копия в `/var/backups/chiit/<path>-<unix_ts>`. Хранится 5 копий (rotate by mtime).
- Whyrun mode: `--why-run` — всё что бы делалось, но not actually.

### K.2 bosun
- Логирование: `tracing` text/json.
- Diff: print_apply / print_plan в text или JSON format.
- Backup dir: `--backup-dir=/var/backups/bosun` (есть). Структура не описана, надо проверить.
- Dry-run: `--dry-run` (есть).

### K.3 gap
- **Failure post-mortem.** chiit `/var/log/chiit-<checksum>.log` загружается на сервер автоматически. bosun: только `/var/log/bosun/`, локально. **Out-of-scope без server'а.**
- **Diff output читаемость.** chiit использует `aryann/difflib` (unified diff). Проверить, что bosun делает на `file.content::ChangeReport`. Дизайн упоминает description="..." как human-readable description — это **хуже** unified diff на больших конфигах.
- **TemplateWithValidation forensics.** chiit при failed валидации оставляет `.new` файл. **bosun дизайн фиксирует то же.** **gap закрыт в дизайне Phase H.**

## Категория L. Прочее

### L.1 archive / tar
chiit не использует.

### L.2 timezone setup
chiit не настраивает (предполагается, что нода уже в UTC).

### L.3 env vars
chiit устанавливает через `EnvironmentFile` в systemd unit (`postgres-chiit/lib/systemd/service.go:104,162-181`). bosun: будет через `runr.service` / `systemd.service` `EnvironmentFiles` (есть в дизайне Phase D).

### L.4 PROFILE setup
chiit не настраивает `/etc/profile` (это apt-package'ов забота).

## Сравнительная таблица провайдеров

| Провайдер chiit | Описание | Статус в bosun | Приоритет |
|---|---|---|---|
| `file.content` | пишет файл атомарно (md5 dedup) | ✅ есть | — |
| `file.template` | через Go template | ✅ есть (minijinja) | — |
| `apt.package` install | ставит точную версию | ✅ есть | — |
| `apt.package` upgrade | без указания версии | ❌ нет | важно |
| `apt.package` remove/purge | удалить пакет | ❌ нет | важно |
| `apt.repository` | добавить .list файл | ❌ нет | **критично** |
| `apt.key` | apt-key add URL | ❌ нет | **критично** |
| `apt.root_repo` | переписать sources.list | ❌ нет | **критично** |
| `apt.update` | принудительно обновить кеш | ❌ нет | **критично** |
| `file.directory` (mkdir+chown) | создать директорию | ❌ нет | **критично** |
| `file.symlink` | симлинки | ❌ нет | **критично** |
| `file.delete` | удалить файл/директорию | ❌ нет | **критично** |
| `file.download` | HTTP GET с MD5 проверкой | ❌ нет | важно |
| `users.user` | создать пользователя | ❌ нет | **критично** |
| `users.group` | создать группу | ❌ нет | **критично** |
| `wait.port` | TCP-port wait | ❌ нет | важно |
| `systemd.service` (NewService) | unit-файл + enable+start | 🔄 Phase E | **критично** |
| `systemd.timer` | timer-юнит | 🔄 Phase E | важно |
| `runr.service` | runr service unit | 🔄 Phase D | **критично** |
| `runr.timer` | runr timer | 🔄 Phase D | важно |
| `runr.cgroup` | runr cgroup | 🔄 Phase D | **критично** для k8s |
| `service.unit` (abstract) | dispatcher | 🔄 Phase F | важно |
| `command.run` | shell-команда | 🔄 Phase G | **критично** |
| `command.run deferred=True` | в defer journal | 🔄 Phase G | **критично** |
| `command.only_if/not_if` | условный exec | 🔄 Phase G | важно |
| `TemplateWithValidation` | render + validate + swap | 🔄 Phase H | **критично** |
| `health_check_cmd/url` | post-restart проверка | 🔄 Phase I | важно |
| `pg_sql.exec` | INSTALL EXT, CREATE ROLE, ... | ❌ нет (только в плане) | **критично** |
| `pg_sql.query` | discovery-факт | ❌ нет | **критично** |
| `cert.tls` | client cert/key/ca | ❌ нет | **критично** |
| `vault.secret` (через server) | http chiit-server | ❌ нет | **критично** |
| `apt.policy_rc_d_101` | install без сервиса | ❌ нет | низко |
| `journald hook` | logrus -> journal | ❌ нет | низко |
| `defers.AddRestart*` | systemd reload defer | 🔄 Phase D | **критично** |
| `defers.AddCommand` | command defer | 🔄 Phase G | **критично** |
| `pacer.Tick` | размазывание CPU | ❌ нет | **критично** |
| `etc_hosts.entry` | hosts injection | ❌ нет (через file.content) | низко |
| `sysctl.value` | sysctl.d + reload | ❌ нет (через file.content) | средне |
| `ssh_known_hosts` | — | ❌ нет | не нужно |
| `kernel.module` | — | ❌ нет | не нужно |
| `mount.point` | — | ❌ нет | не нужно |
| `iptables` / `ufw` | — | ❌ нет | не нужно |
| `lvm.volume` | — | ❌ нет | не нужно |
| `archive.extract` | — | ❌ нет | не нужно |
| `git.checkout` | — | ❌ нет | не нужно |
| `cron.entry` | — | ❌ нет (через runr.timer) | низко |
| `tunefs (tune2fs)` | — | ❌ нет (есть в роли через shell) | низко |
| `p2p re-exec` | self-upgrade peer | ❌ нет | **критично** |
| `self-upgrade binary` | apt + restart | ❌ нет | **критично** |
| `silence host` | alertmanager API call | ❌ нет (custom command) | низко |
| `pod IO/CPU limits` | direct /sys/fs/cgroup | ❌ нет (runr.cgroup частично) | важно |
| `warden discovery` | k8s pod-discovery | ❌ нет | **критично** для server |
| `chiit-server HTTP client` | bundle/vault/cert | ❌ нет | **критично** для server |
| `storage_inventory client` | per-cluster overrides | ❌ нет | **критично** для server |
| `canary rollout severity` | hash-based selection | ❌ нет | **критично** |
| `inventory MERGE strategies` | deep merge | ✅ есть | — |
| `patroni-guard sync` | http://127.0.0.1:8009 | ❌ нет | **критично** для PG |

## Сравнительная таблица фактов

| Факт chiit | Описание | Статус в bosun | Приоритет |
|---|---|---|---|
| hostname | `hostname -f` или POD_FQDN | ✅ есть | — |
| host_group | first segment до `-` | ❌ нет | низко (compute fn) |
| memory_mb | /proc/meminfo или cgroup | ✅ есть | — |
| cpu_count | runtime.NumCPU или cgroup | ✅ есть | — |
| outbound_ip | UDP-Dial 8.8.8.8 trick | ❌ нет | низко |
| is_pod | os.Stat /etc/pod.conf | ✅ есть | — |
| is_burstable_pod | pod_qos != Guaranteed | ❌ нет | важно (для k8s policy) |
| is_runr_init | POD_INIT=runr | 🔄 Phase D | — |
| ubuntu_release | /etc/lsb-release | ❌ нет | важно (Bionic-fallback) |
| init_system | systemd/runit/runr | ✅ есть | — |
| installed_packages | dpkg/status parse | ✅ есть | — |
| pg_is_master | SELECT pg_is_in_recovery() | ❌ нет | **критично** |
| pg_users_with_passwords | discovery query | ❌ нет | **критично** |
| pg_extensions | discovery | ❌ нет | важно |
| pg_initialized | os.Stat pg_control | ❌ нет | важно |
| dm_device | /proc/mounts + /sys/devices | ❌ нет (PG/IO-limit specific) | важно для k8s |
| persons | itc-info.yaml | ❌ нет | низко (PG-info специфично) |

## Архитектурные gap'ы

### Bootstrap / server взаимодействие
- bosun-server полностью отсутствует.
- bosun-client пока не умеет:
  - Получать bundle с удалённого источника (S3/OCI/HTTP).
  - Получать секреты с server'а (Vault через proxy).
  - Получать сертификаты с server'а.
  - Загружать audit-log на server.
  - Реагировать на push-rollout.
  - Self-upgrade.

### Self-upgrade
- chiit имеет `cmd/upgrade.go` (severity-aware download + atomic rename + chmod) и `lib/p2p/server.go` (раздача бинаря соседям, re-exec на HUP).
- bosun ничего из этого не имеет.
- **Без self-upgrade на 40-60k нод нет миграции** — каждое обновление потребует mssh push.

### Failure recovery
- DPKG lock recovery — есть в bosun (Phase D).
- Crash mid-apply: bosun имеет defers journal с at-least-once.
- Network glitch — bosun имеет retry в `is_deferrable`.
- Self-upgrade rollback — нет (chiit тоже не имеет, но в chiit upgrade — отдельный flow).

### Throttling / coordination
- pacer: chiit размазывает 4-секундный run до 30-секундного через `pacer.Tick()` 60-100ms между шагами. Без этого 60k нод одновременно ддосят apt-зеркала.
- bosun: ничего. **gap критично.**

## Приоритизация

### 1. Блокирующие замену chiit в проде (без чего категорически нельзя)
1. **`apt.repository` + `apt.key` + `apt.root_repo` + `apt.update`** — без PG-репозитория не поставить ничего PG-специфичного.
2. **`directory.tree` (mkdir+chown+chmod recursive)** — каждый ресурс требует.
3. **`file.symlink`** — PG nix-paths, patroni, runr.
4. **`file.delete`** — repo cleanup, кеш cleanup, journald override delete.
5. **`users.user` + `users.group`** — postgres user.
6. **`apt.package` upgrade без версии** — security upgrades (risk.go).
7. **`apt.package` remove/purge** — uninstall ненужного.
8. **Phase D, E, F (runr/systemd/service.unit) + дефер-журнал** — service management.
9. **`command.run` + `only_if/not_if` + `deferred=True`** (Phase G) — без него ½ ролей не работают.
10. **`validate_with` (Phase H)** — production safety.
11. **`pg_sql.exec` + DSN connection** — нет ни одного PG-пользователя без него.
12. **Vault/secret HTTP-клиент к chiit-server** — нет паролей → нет PG.
13. **`cert.tls` provider** — нет SSL → нет patroni cluster.
14. **patroni-guard sync (HTTP GET 127.0.0.1:8009 polling)** — без него split-brain в PG.
15. **pacer / throttling** — без него apt-mirror DDoS.
16. **Inventory merge from chiit-server (storage_inventory)** — без него per-cluster overrides не работают.
17. **canary rollout (severity classes + hash-based)** — без него нельзя катить безопасно.
18. **Self-upgrade механизм** — без него миграция невозможна.

### 2. Желательное для первого деплоя (операционная боль без них)
1. `wait.port` — без него race PG-инициализация.
2. `file.download` — для apt-key (или через command.run).
3. `health_check_cmd` (Phase I) — post-deploy валидация.
4. `is_runr_init` / `ubuntu_release` / `is_burstable_pod` facts.
5. journald output (хотя бы fallback).
6. `--severity-file` CLI флаг + report logging.
7. `--patch-inventory` для testing-overrides.
8. `bosun upgrade` subcommand.

### 3. Можно сделать позже (не критично на старте)
1. `etc_hosts.entry` — закрывается через `file.content` для simple cases.
2. `sysctl.value` — закрывается через `file.content` + `command.run`.
3. `cron.entry` — если все ноды jammy+runr → не нужно.
4. tune2fs — через `command.run`.
5. p2p re-exec — можно начать с прямого download.
6. `apt.policy_rc_d_101` — только для major-upgrade flow.
7. `silence_host` (alertmanager) — через `command.run`.

## Открытые вопросы для пользователя

1. **bosun-server scope:** Эта итерация ресерча была client-only. Однако ¾ критических gap'ов (vault, cert, inventory storage_inventory, canary rollout, bundle distribution, self-upgrade) требуют server-side компонент. Что планируется первым — выкатывать bosun без server (использовать chiit-server как-есть с HTTP-клиентом? через какой URL?) или делать сразу два компонента? Это определяет минимальный набор примитивов для пилота.

2. **PG SQL-операции — отдельный примитив или через `command.run`?** Можно завести `pg_sql.exec(dsn, query)` (native, типа `apt.package`) или ограничиться `command.run(["psql", "-h", "/var/run/postgresql", "-c", "CREATE ROLE..."])`. Первый идиоматичнее и даёт diff'ы (но требует постоянной поддержки DSN-пуллинга, lib/pq аналога). Второй простой, но diff невозможен — каждый `command.run` всегда "changed".

3. **PG discovery-facts (pg_is_master, pg_users) — есть план?** Сейчас в bosun-facts только 6 базовых. Не очерчена политика, как добавлять PG-specific факты — внутри bosun-facts крейта (хардкод) или через extension-механизм / plugin?

4. **Self-upgrade flow:** Bundle обновляется через bosun-server push? Сам бинарь bosun-client обновляется как? Через apt-пакет (тогда нет нужды в самообновлении) или как chiit через self-download? Если apt — то откуда apt узнаёт о новой версии: bundle сам ставит новый apt-пакет bosun?

5. **patroni-guard:** Это отдельный сервис на ноде, не имеющий ничего общего с bosun. Будет ли bosun явно ждать его готовности (HTTP GET с retry на старте patroni-роли) или patroni-guard переписывается под bosun и интегрируется как primitive?

6. **chiit-server совместимость:** Если bosun-client ходит к старому chiit-server (для миграционного периода) — нужны ли совместимые эндпоинты или планируется параллельный новый bosun-server рядом? Это влияет на формат секретов (vault path schema), audit-log и canary endpoint.

7. **pacer-эквивалент:** Нужен ли распределённый pacer внутри bosun-client (как chiit, на стороне ноды) или это переезжает на server-side rollout-rate? Текущий chiit-подход размазывает на 30s per node, что **client-side**. Без чего-то подобного 60k нод одновременно ддосят сервисы.

8. **bionic-ноды:** Парк включает Bionic-ноды без runr. Должен ли bosun MVP поддерживать **только jammy+runr-init** или сразу оба варианта? Это влияет на `cron.entry` примитив, `is_runr_init` факт, dual systemd/runr API.

9. **k8s vs KVM dispatch:** `service.unit` дизайн (Phase F) делает dispatch по `init_system` факту. Open question: что делать с гипервизорами, где systemd + runr сосуществуют (`mixed-systemd-runr`)? Дизайн упоминает "primary=systemd" но в реальности патrони и pg-сервисы — runr.

10. **Тестовая среда для PG-сценариев:** Cucumber-rs BDD в Docker уже есть. Нужно ли расширить до tests с полноценным postgres + patroni + pgbouncer (test-cluster)? Это **значительная инвестиция** в test-infrastructure, но без неё bosun не дойдёт до полной замены chiit.
