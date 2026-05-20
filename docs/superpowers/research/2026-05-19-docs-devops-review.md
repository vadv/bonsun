# DevOps-review operator documentation для bosun-client
Дата: 2026-05-19
Reviewer: DevOps-инженер впервые видящий bosun

## TL;DR

1. README и bundle-authoring выглядят как минимум на 80% готовыми для опытного оператора: терминология определена, exit codes таблицей, метрики и алерты есть. Но новичок не сможет пройти Quick start as-is — нет инструкции «откуда взять исходники», `cargo` не упомянут как зависимость, нет проверки на rustc-toolchain.
2. Operator runbook покрывает счастливые сценарии (deadline, SIGTERM, reboot), но имеет дыры в production-критичных моментах: нет описания первичной установки на голую ноду (без cargo, без интернета), нет требований к OS (Debian-only намёками, но Alpine/RHEL вообще не упомянуты), нет ничего про observability вне Prometheus textfile, нет log-rotation, нет capacity planning для `/var/backups/bosun`.
3. Примеры с runr выглядят академически: pgbouncer-cluster требует «runr daemon запущен на 127.0.0.1:8010», но runr не существует ни в дистрибутивах, ни как готовый пакет — оператор не сможет повторить пример. Дисклеймер «runr живёт в отдельном репозитории» нужен где-то рядом.
4. **Bundle authoring guide отличный и почти полный** — там и notify-каналы, и defers lifecycle с at-least-once, и validation `{new_path}`, и health-check sync vs async. Это сильное место документации.
5. Безопасность затронута поверхностно: «требует root» сказано, но нет рекомендаций по secrets handling (`pg_sql.exec` с паролем в bundle.toml!), нет про SELinux/AppArmor, нет про шифрование bundle при доставке.

## Detailed findings

### A. New-user experience

- [x] **Pro:** README:1-7 — одного абзаца достаточно, чтобы понять «это chef/puppet/ansible на Rust для PG-парка». Аналогия мгновенная.
- [x] **Pro:** README:9-16 — различие `bosun apply` (local) vs `bosun connect` (server-managed) объяснено сразу. Кейсы для local чёткие.
- [x] **Pro:** README:188-219 — exit codes и флаги таблицей. Reference-уровень читаемости.
- [ ] **Issue:** README:20-26 — `cargo install --path crates/bosun-cli` подразумевает Rust toolchain. Нет «Prerequisites: Rust 1.XX+». Не-разработчик остановится здесь. Также нет упоминания `git clone <repo>` — команда читается «откуда-то».
- [ ] **Issue:** README:36-39 — Makefile-цели `musl-*` упомянуты без объяснения зависимостей (`cross`? `musl-tools`?). Магия.
- [ ] **Issue:** README:101 — Quick start не вводит «всегда сначала dry-run» как правило; порядок `dry-run` затем `apply` подан как один из вариантов, а не workflow.
- [ ] **Missing:** термины «advisory-lock через `flock`» (README:201), «musl-бинарь» (README:32), «cgroup-aware» (bundle-authoring:633) используются до определения. Glossary нет.
- [ ] **Missing:** в Quick start нет `bosun version` или `bosun --help` — типичные первые команды.
- [ ] **Missing:** для `requires_bosun = "^0.1"` нужно знание cargo-style semver (caret), без объяснения.

### B. Полнота operational guidance

- [x] **Pro:** runbook:62-110 — три варианта periodic execution (runr/systemd/cron) с конкретными unit-файлами и обоснованием `RandomizedDelaySec`.
- [x] **Pro:** runbook:134-149 — lock на SIGKILL объяснён; «exit 0 если другой инстанс работает» предотвращает ложные алерты.
- [x] **Pro:** runbook:200-219 — готовые PromQL алерты с обоснованием «attempt а не run».
- [ ] **Issue:** runbook:14-39 — раздел «Из исходников» — workstation-flow «соберите, scp». Apt-секция говорит «сборка вне репозитория». Итог: production-deployment не покрыт нигде, оператор должен сам строить CI/CD.
- [ ] **Issue:** runbook:24 — `make bosun-bookworm` упомянут как готовая цель, но её существования в README не подтверждено.
- [ ] **Issue:** runbook:64-68 — `[Service]` блок runr-юнита с `User = root`. Синтаксис runr-юнитов нигде не описан, отсылок к docs runr нет.
- [ ] **Issue:** runbook:152-191 — нет log-rotation: сколько места займут логи через 30 дней при таймере раз в 5 минут?
- [ ] **Issue:** runbook:270-280 — `/var/backups/bosun/` растёт бесконечно. Logrotate-рекомендация без примера конфигурации.
- [ ] **Missing:** **OS support matrix**. Намекается Debian/Ubuntu (`apt`, `dpkg`, `bookworm`), но Alpine/RHEL/SUSE — статус неизвестен.
- [ ] **Missing:** **Hardware requirements**. RAM minimum, disk для state/log/backup, DNS-зависимость — нет.
- [ ] **Missing:** **Security section отсутствует целиком**. `pg_sql.exec` принимает DSN с паролем (bundle-authoring:781), а bundle лежит в git — это плохо. SELinux/AppArmor — статус не описан. Доставка bundle — какой канал, шифрование? Firewall — нужен ли rule для 127.0.0.1:8010?
- [ ] **Missing:** observability за пределами Prometheus textfile. `--log-format=json` есть, но рекомендации «через vector/promtail/fluentbit» нет.
- [ ] **Missing:** runbook:392-415 «Status quo» — by-design ограничения без workaround-рекомендаций.
- [ ] **Missing:** **Grafana dashboard JSON** — алерты есть, графиков нет.

### C. Bundle authoring completeness

- [x] **Pro:** bundle-authoring:177-269 — notify-каналы с таблицей `depends_on/reload_on/restart_on` и full-example. Самое сильное место документации.
- [x] **Pro:** bundle-authoring:271-376 — defers lifecycle с шестью шагами, priority sortkey `0r/1r/2r/3c/4d`, dedup правилами, обоснованием tmpfs.
- [x] **Pro:** bundle-authoring:378-444 — validation pattern с `{new_path}`, lifecycle на `file.content` и `service.unit` отдельно.
- [x] **Pro:** bundle-authoring:445-531 — health-check sync (на Start) vs async (через defer-replay), retry policy таблицей.
- [x] **Pro:** bundle-authoring:646-820 — каталог 21 примитива, систематично, сигнатура и пример для каждого.
- [ ] **Issue:** bundle-authoring:773-786 — `pg_sql.exec` пример с **паролем в открытом виде** (`CREATE ROLE monitoring WITH LOGIN PASSWORD 'changeme'`). Нет рекомендации vault/inventory-secret. Bad security example.
- [ ] **Issue:** bundle-authoring:701-715 — `users.user(name="pgbouncer", system=True)` пример. Не объяснено, что пкг `pgbouncer` сам создаёт пользователя — типичный конфликт.
- [ ] **Issue:** bundle-authoring:667-672 — `apt.key` без примера. Нет «как добавить PGDG-репозиторий» — центральный кейс PG-парка.
- [ ] **Issue:** bundle-authoring:751-769 — `process.signal` идемпотентность при нескольких процессах под одним именем не описана.
- [ ] **Missing:** **полный apt repo + key + package pipeline для PG**. Кусочки есть, готового sequence нет.
- [ ] **Missing:** **cross-resource notify через несколько ролей**. Пример показывает intra-role notify. Передача handle между ролями (A пишет конфиг, B управляет сервисом) — не описано.
- [ ] **Missing:** **canary rollout «10% нод»** — bundle-authoring:580-603 даёт severity_class, но «hash hostname'а» оставлен на deployment-system. Готового рецепта `hostname | hash | mod 100 → severity_class` нет.
- [ ] **Missing:** **template debugging** — где смотреть промежуточный Jinja2-рендеринг при `--dry-run`?

### D. Format и readability

- [x] **Pro:** code fences с правильной подсветкой (`starlark`, `toml`, `ini`, `sh`, `yaml`, `jinja`).
- [x] **Pro:** таблицы reference-материала читаемы, не разъезжаются, дефолты явно.
- [x] **Pro:** heredoc'ов `cat <<EOF` нет — заменены на code fences.
- [x] **Pro:** AI-маркеров «leverages/эффективно/комплексный» практически нет.
- [ ] **Issue:** Diataxis-разделение нечёткое: README смешивает concept + tutorial + reference под одной крышей. Bundle-authoring (4000 слов) — смесь tutorial/how-to/reference, поиск конкретного ответа долгий.
- [ ] **Issue:** bundle-authoring:631-641 — `is_pod` в таблице фактов без определения. Сленг.
- [ ] **Issue:** examples/multi-role-pg/README.md:132-138 — блок кода с 4-space indentation вместо code fence. Несогласованно.

### E. Conceptual deep dives

- [x] **Pro:** **Defers lifecycle** (bundle-authoring:291-315) — полный flow с at-least-once. Priority sortkey с объяснением «почему числовой».
- [x] **Pro:** **Dedup правила** (bundle-authoring:337-348) — три случая с примерами.
- [x] **Pro:** **Notify-каналы** (bundle-authoring:177-244) — таблица поведения parent.Failed/Changed/Unchanged.
- [x] **Pro:** **Validation pattern** — `.new` для forensics, substitution ограничен `{new_path}` сознательно.
- [x] **Pro:** **Health-check sync vs async** — sync на Start, async через defer-replay для Restart/Reload.
- [ ] **Issue:** bundle-authoring:296-302 — atomic write `tmp → fsync(file) → rename → fsync(dir)`. Поведение при отказе диска не упомянуто.
- [ ] **Issue:** bundle-authoring:351-363 — tmpfs by design без оценки «сколько занимает journal на тысяче defers» (важно для `/tmp` = 1G).
- [ ] **Issue:** bundle-authoring:580-610 — severity_class в inventory через «hash hostname'а deployment-системой» — handwave без конкретного скрипта.
- [ ] **Missing:** **bundle между прогонами**. Старые defers с прежним `health_check_spec` — выполнятся с новой логикой? Не описано, лежит ли спека в `DeferEntry` или подтягивается заново.

### F. Examples completeness

- [x] **Pro:** **nginx-demo** — starter-example. Prerequisites чёткие, outcome с метриками, verification рабочий.
- [x] **Pro:** **pgbouncer-cluster** — production-like с notify/validate/health-check.
- [ ] **Issue:** **multi-role-pg** — «не управляет состоянием сервисов» (README:77-80). Вводит в заблуждение оператора, ищущего working PG-setup. Нужен дисклеймер на уровне корня.
- [ ] **Issue:** **pgbouncer-cluster** требует «runr daemon на 127.0.0.1:8010» — но как его запустить, откуда взять, не описано. Ссылок на runr-репозиторий нет.
- [ ] **Issue:** **pgbouncer-cluster** на systemd-ноде всё равно пишет `/etc/runr/pgbouncer.service` (README:131-139). Файл лежит впустую — путаница для оператора.
- [ ] **Missing:** **dev-машина без побочных эффектов**. Все примеры предполагают «голую ноду» — на рабочем ноутбуке с установленным nginx/pgbouncer они сломают существующее. Sandbox-режим нужен явный.
- [ ] **Missing:** **Verification timeline**. `pg_isready` после apply (pgbouncer-cluster README:101) — а bosun завершился до того, как async-health-check через defer-replay снял запись. Когда конкретно проверять — «сразу» или «через X секунд»?

## Конкретные блокеры для production-использования

1. **OS support matrix отсутствует.** Apt/dpkg намекают на Debian-only, но Alpine/RHEL/SUSE — статус неизвестен. Блокирует выбор для гетерогенного парка.
2. **Установка на голую ноду требует Rust toolchain.** Quick start = `cargo install`, на голой ноде Rust не стоит. Musl-бинари упомянуты, но «откуда взять артефакт» не описано. Apt-пакет — «вне репозитория».
3. **Secrets handling.** `pg_sql.exec` пример с паролем в открытом виде. Нет рекомендации по vault/secrets. Для PG-парка пароль superuser-роли не должен быть в git.
4. **Runr daemon как dependency без инструкции.** Pgbouncer-cluster требует runr на 127.0.0.1:8010, но откуда его взять — не описано. Вакуум.
5. **Bundle distribution out of scope.** Runbook:404-407: «доставка — следующая итерация». Без доставки инструмент неприменим. Нужна хотя бы временная рекомендация (git pull в systemd-таймере до bosun.timer).
6. **Log/backup retention отсутствует.** `/var/log/bosun/` и `/var/backups/bosun/` растут бесконечно. Готовой logrotate-конфигурации нет — диск переполнится за месяц.
7. **Cross-bundle notify identity** (runbook:408-410) блокирует разделение PG-стэка по командам (отдельные bundle'ы postgres/patroni/pgbouncer).
8. **Health-check без warmup.** Sync health-check (bundle-authoring:464-470) упадёт, если сервис начинает слушать порт через 30 секунд после старта.
9. **Несогласованность пример vs реальность.** Pgbouncer-cluster пишет `/etc/runr/pgbouncer.service` на systemd-нодах вхолостую (README:131-139).
10. **`bosun connect` упомянут как production-режим (README:9-16), но не существует в этом крейте.** Ссылки на bosun-server-репозиторий нет.

## Конкретные рекомендации с приоритетом

### High (надо до production)

- **Prerequisites** в README: OS-list, Rust версия для сборки, либо «возьмите готовый musl-бинарь по ссылке». Сейчас нет точки входа для не-разработчика.
- **OS support matrix** в первой секции runbook.
- **Security section** в operator-runbook: secrets в bundle (vault), доставка bundle (signed/encrypted), systemd-unit с минимальными `CapabilityBoundingSet`, SELinux/AppArmor статус.
- **Log-rotation / backup-retention пример** (`/etc/logrotate.d/bosun`).
- **Deployment без cargo**: ссылка на release-артефакт или apt-package с явным репозиторием.
- В pgbouncer-cluster добавить **ссылку на runr-репозиторий** или явное «runr пока не готов, используйте systemd».
- **Secrets pattern** в bundle-authoring: пароль PG-роли через файл `/etc/bosun/secrets/` или env.

### Medium (UX-фрустрация)

- **Готовый Grafana dashboard JSON** в `dashboards/bosun.json`.
- **Canary rollout с конкретным hash-скриптом** (`hostname | md5sum | head -c 2 | hex2int → severity_class`).
- В runbook **decision tree «алерт → что делать»**: BosunStale → проверьте таймер; BosunApplyFailed → exit code → debug; BosunManualClearPending → `bosun status`.
- **Разделить bundle-authoring** на cookbook (how-to) + reference (каталог примитивов).
- **Template debugging** — как смотреть промежуточный Jinja2-рендеринг.
- В multi-role-pg README **дисклеймер** «это layout-пример, не working PG-setup».
- **Verification timing**: ждать ли после `bosun apply` exit 0 перед `pg_isready`?
- **Полный apt repo + key + package pipeline для PGDG** — типичный PG-парк use case.

### Low (nice-to-have)

- **CHANGELOG.md** для требований bosun-bundles.
- **Diataxis-маркировка** секций: пометить README:18-117 как Tutorial, README:119-228 как Reference.
- В каталоге примитивов **«typical pitfalls»**: например, `users.user` vs apt-пакет, создающий user.
- **Glossary**: musl, advisory-lock, runr, cgroup-aware.
- **Rollback стратегия** для версий бинаря bosun.
- **Migration guide** Ansible/Puppet → bosun: можно ли вместе на одной ноде, кто за что отвечает.

## Что особенно понравилось

1. **Defers lifecycle (bundle-authoring:271-376)** — шесть шагов от enqueue до success с at-least-once, числовые префиксы `0r/1r/2r/3c/4d` с объяснением «почему цифры, а не буквы». Уровень Postgres internals.
2. **Notify-каналы таблицей (bundle-authoring:226-235)** — три типа с поведением на parent.Failed/Changed/Unchanged. Устраняет путаницу с Ansible/Puppet notify-семантикой.
3. **Validation pattern с `{new_path}` (bundle-authoring:378-444)** — `.new` для forensics, substitution ограничен сознательно. Инженерное решение, объяснённое.
4. **Exit codes таблицей (README:222-229)** — 0/1/2/3/4/130 с смыслом каждого. Для CI-pipeline'ов инструмент номер один.
5. **PromQL алерты (runbook:200-219)** с обоснованием «attempt а не run». Редкая глубина в operations-докуах.
6. **Status quo секция (runbook:392-415)** — честный список by-design ограничений. Оператор сразу видит границы.
7. **Tmpfs by design (bundle-authoring:351-363)** — обоснование с инженерной аргументацией (`fsync(dir)` на tmpfs = memory barrier) и escape hatch (`--defers-dir`). Уровень «понимает свои предположения».
