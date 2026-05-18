# nginx-demo

Минимальный bundle bosun-client'а: ставит nginx и рендерит
`/etc/nginx/nginx.conf` из шаблона. Демонстрирует три основные возможности
MVP — установку пакета, генерацию файла из шаблона и доступ к фактам
ноды (`inv.facts.hostname`).

## Запуск в Docker

Соберите бинарь bosun под bookworm-slim glibc:

    make bosun-bookworm

Постройте образ-сборку:

    docker build -t bosun-test-base:latest -f docker/test-base.Dockerfile .

Запустите контейнер и примените bundle:

    docker run --rm -v $(pwd):/work bosun-test-base:latest bash -c '
        cp /work/target/bookworm-release/bosun /usr/local/bin/bosun
        bosun apply --bundle /work/examples/nginx-demo/bundle \
          --lock-path /tmp/bosun.lock \
          --state-dir /tmp/bosun-state \
          --log-dir /tmp/bosun-log \
          --backup-dir /tmp/bosun-backups \
          --metric-file /tmp/bosun.prom
        dpkg-query -W nginx
        cat /etc/nginx/nginx.conf
    '

## Структура

- `bundle.toml` — метаданные bundle'а (name, version, requires_bosun, entry)
- `manifests/main.star` — Starlark-декларация ресурсов
- `defaults/main.yaml` — переменные inventory с дефолтами
- `templates/nginx.conf.j2` — minijinja-шаблон конфига

## Override inventory

Чтобы переопределить `worker_processes`, передайте локальный override:

    cat > /tmp/inv.yaml <<EOF
    worker_processes: 4
    EOF
    bosun apply --bundle ./examples/nginx-demo/bundle --inventory /tmp/inv.yaml ...

Семантика merge — deep-merge с null-удалением: `null` в override на
ключе удаляет этот ключ из defaults; отсутствие ключа — оставляет defaults
как есть.

## Пинить версию nginx

В текущем `main.star` версия не пинится — apt подбирает то, что доступно
в текущем mirror'е. Если нужен жёсткий пин, замените `apt.package(name = "nginx")`
на:

    apt.package(
        name    = "nginx",
        version = inv.nginx_version,
    )

и задайте в `defaults/main.yaml`:

    nginx_version: 1.22.1-9+deb12u6

Проверьте доступность версии: `apt-cache madison nginx` внутри
debian:bookworm-slim. При несовпадении (например, security-обновление
сменило suffix) bosun зафиксирует drift на каждом прогоне.

## dry-run

Чтобы посмотреть план без apply:

    bosun apply --bundle ./examples/nginx-demo/bundle --dry-run ...

Exit-code 2 означает «есть drift», 0 — «всё уже в нужном состоянии».
