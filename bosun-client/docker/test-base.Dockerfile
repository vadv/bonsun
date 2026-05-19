FROM debian:bookworm-slim

# python3 нужен для BDD-сценария apt_package.feature: держатель
# fcntl-локa /var/lib/dpkg/lock-frontend через `python3 -c "fcntl.lockf(...)"`.
# Без него F03-фикс не покрыт на docker-уровне (apt/dpkg используют fcntl,
# не flock).
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
    ca-certificates curl python3 \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /work
