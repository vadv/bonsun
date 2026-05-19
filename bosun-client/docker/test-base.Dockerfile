FROM debian:bookworm-slim

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
# - dbus-daemon + python3-dbusmock — заготовка для systemd.service-сценариев,
#   которые требуют поднятого system bus с mock'ом org.freedesktop.systemd1.
#   Без --privileged настоящий systemd в контейнере не запустится, но mock
#   через dbus-daemon --system работает.
# - postgresql-client (psql) — для pg_sql.exec/pg_sql.query сценариев,
#   когда docker-compose поднимает реальный postgres рядом.
# - runr: настоящий supervisor-демон из локального проекта runr. Бинарь
#   собирается отдельно через `make runr-bookworm` в том же
#   rust:1-bookworm-контейнере, что и bosun, чтобы GLIBC совпал с базой
#   образа. Сценарии @runr-service поднимают его через docker exec
#   (`runr supervisor &`), сами создают /etc/runr/<name>.service и
#   проверяют реальные ответы HTTP API на 127.0.0.1:8010.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
    ca-certificates curl python3 python3-dbusmock \
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

# Реальный runr supervisor. См. Makefile target `runr-bookworm` —
# артефакт собирается из локальных исходников и кладётся в
# target/runr-bookworm/runr. Если файл отсутствует, сборка docker-base
# падает с понятным сообщением: запускайте `make runr-bookworm` или
# целиком `make test-bdd`, который тянет цепочку зависимостей.
COPY target/runr-bookworm/runr /usr/local/bin/runr
RUN chmod +x /usr/local/bin/runr \
 && mkdir -p /etc/runr /var/log/runr

WORKDIR /work
