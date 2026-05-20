# Multi-stage сборка: `base` собирается без runr-binary и покрывает все
# BDD-сценарии, кроме `@runr-service`. Stage `with-runr` поверх `base`
# добавляет настоящий supervisor-бинарь для сценариев, которые поднимают
# runr daemon внутри контейнера. CI на public-репе без приватного
# RUNR_REPO_URL собирает `--target=base` и прогоняет ~80 сценариев без
# runr; локальный dev и CI с RUNR_SOURCE_DIR собирают `--target=with-runr`
# и покрывают всё.
FROM debian:bookworm-slim AS base

# Базовый образ для BDD-сценариев bosun. Все примитивы, которые имитируют
# реальные системные действия (useradd, sysctl, pkill, gpg --show-keys,
# sysctl, sha256sum), требуют присутствия соответствующих утилит. Без них
# scenario падает не на assertion'е, а на «not found», и оператор видит
# не симптом, а помеху.
#
# Состав диктуется Phase K (BDD docker) и формально перечислен в плане:
# - apt/dpkg/gpg → apt.package, apt.key, apt.update_cache.
# - python3 → держатель fcntl-локa /var/lib/dpkg/lock-frontend и парсер
#   defer-файлов в тестах (см. defers.rs::defer_file_content).
# - procps (pkill/ps) → process.signal по имени/uid.
# - passwd-suite (useradd/groupadd/userdel/groupdel + getent) → users.user,
#   users.group; в debian:bookworm-slim они уже есть, оставляем явно
#   для документации зависимостей.
# - sysctl → sysctl.reload (procps содержит).
# - jq → удобный grep по JSON-выводу `bosun status --format json` и по
#   payload defer-файлов в шагах сценария.
# - openssl → проверка cert.tls сертификатов (CN, validity) без
#   зависимости от Rust-парсера.
# - dbus-daemon — system bus, через который bosun-systemd-client общается с
#   PID 1. systemd-as-PID1 поднимается только в @systemd-privileged
#   сценариях (см. test-bdd-systemd); обычный test-bdd запускает контейнер
#   с `tail -f /dev/null`, dbus-daemon никем не используется, но пакет
#   остаётся, чтобы systemd-в-PID1-режиме мог стартовать без доустановки.
# - postgresql-client (psql) — для pg_sql.exec/pg_sql.query сценариев,
#   когда docker-compose поднимает реальный postgres рядом.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
    ca-certificates curl python3 \
    dbus dbus-bin dbus-user-session \
    procps psmisc passwd \
    gnupg2 jq openssl \
    postgresql-client \
 && apt-get clean \
 && rm -rf /var/cache/apt/archives/*
# Намеренно НЕ удаляем /var/lib/apt/lists/* — apt.package сценарии
# зависят от свежего кеша, а apt-get update в чистом контейнере без
# списков занимает ~30 секунд, что превышает дефолтный per-attempt
# timeout (30s) в apt_package::recovery. Сохранение кеша сокращает
# время каждого `bosun apply` с apt.package до миллисекунд.

# Каталоги для runr создаются и в base-stage. `@runr-service` сценарии
# монтируют сюда unit-файлы через `docker exec`, и пустые пути не мешают
# другим сценариям. Это даёт `with-runr` только COPY бинаря.
RUN mkdir -p /etc/runr /var/log/runr

# Для systemd-as-PID1-режима (`@systemd-privileged` сценарии). systemd
# слушает SIGRTMIN+3 как «graceful exit для контейнера» — без этого
# `docker stop` ждёт 10 секунд default-таймаута SIGTERM, прежде чем
# отправить SIGKILL. Для обычного PID1 = `tail -f /dev/null` сигнал
# тоже валиден (просто игнорируется), так что объявление безопасно
# для всех сценариев.
STOPSIGNAL SIGRTMIN+3

WORKDIR /work

# Stage `with-runr` — добавляет настоящий runr supervisor поверх base.
# Бинарь собирается отдельно через `make runr-bookworm` в том же
# rust:1-bookworm-контейнере, что и bosun, чтобы GLIBC совпал с базой
# образа. Сценарии @runr-service поднимают его через docker exec
# (`runr supervisor &`), сами создают /etc/runr/<name>.service и
# проверяют реальные ответы HTTP API на 127.0.0.1:8010.
FROM base AS with-runr
COPY target/runr-bookworm/runr /usr/local/bin/runr
RUN chmod +x /usr/local/bin/runr
