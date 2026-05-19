# nginx-demo

Минимальный bundle bosun-client'а: ставит nginx, рендерит `/etc/nginx/nginx.conf`
из шаблона. Демонстрирует role-based layout, `inventory.read` + `inventory.merge`,
module-relative `template()`.

## Что делает example

1. Загружает `inventory/base.yaml` (worker_processes, worker_connections).
2. По тэгу `production` или `staging` merge'ит соответствующий overlay.
3. Через роль `nginx` ставит пакет nginx нужной версии и рендерит
   `/etc/nginx/nginx.conf` из `roles/nginx/templates/nginx.conf.j2`.

## Prerequisites

- Debian-based нода с apt и dpkg.
- Root-доступ (apt и запись в `/etc/nginx/`).
- Бинарь `bosun` в `$PATH` или указан полным путём.
- В Docker — образ из `docker/test-base.Dockerfile` (Debian bookworm с
  предзагруженным apt-кешем).
- На ноде нет накопленных bosun-defers с прошлых прогонов в
  `/tmp/bosun-defers/` (иначе они выполнятся в pre-replay).

## Команда для запуска

Локально под root:

    bosun apply --bundle ./bundle --tags=production

В Docker (для CI/smoke):

    make bosun-bookworm
    docker build -t bosun-test-base:latest -f docker/test-base.Dockerfile .
    docker run --rm -v $(pwd):/work bosun-test-base:latest bash -c '
        cp /work/target/bookworm-release/bosun /usr/local/bin/bosun
        bosun apply --bundle /work/examples/nginx-demo/bundle \
            --tags=production \
            --lock-path /tmp/bosun.lock \
            --state-dir /tmp/bosun-state \
            --log-dir /tmp/bosun-log \
            --backup-dir /tmp/bosun-backups \
            --metric-file /tmp/bosun.prom
    '

## Expected outcome

1. На ноде установлен пакет `nginx` версии из `inventory/base.yaml`
   (`nginx_version: 1.22.1-9`).
2. Файл `/etc/nginx/nginx.conf` существует, owner=root, group=root,
   mode=0o644.
3. Содержимое отрендерено из `nginx.conf.j2` с подставленными
   `worker_processes` и `worker_connections` из inventory.
4. Exit 0. Метрика в `/tmp/bosun.prom` (или дефолтный путь):
   `bosun_resources_total{outcome="changed"} 2` на первом прогоне,
   `bosun_resources_total{outcome="unchanged"} 2` на повторном.
5. Журнал defers пуст (`bosun status` — `pending: 0`).

## Verification

    dpkg-query -W -f '${Status} ${Version}\n' nginx
    # ожидаем: install ok installed 1.22.1-9

    head -5 /etc/nginx/nginx.conf
    # worker_processes auto;
    # events { worker_connections 768; }

    bosun apply --bundle ./bundle --tags=production --dry-run
    # exit 0 — drift'а нет

## Структура

    bundle/
    ├── bundle.toml                       # name, version, requires_bosun, tags
    ├── main.star                         # entry: грузит inventory + роль nginx
    ├── inventory/
    │   ├── base.yaml                     # дефолты
    │   ├── production.yaml               # overrides для tags=production
    │   └── staging.yaml                  # overrides для tags=staging
    └── roles/nginx/
        ├── main.star                     # def configure(inv): apt + file.content
        └── templates/nginx.conf.j2

## Тэги

Манифест вызывает `tags.require_one_of("production", "staging")`. Без
флага `--tags` или с неизвестным значением CLI вернёт exit 3.

## Валидация без apply

    bosun bundle validate --bundle ./bundle --tags=production

Печатает `evaluate OK, N resources registered` (exit 0) или диагностику
(exit 3).

## dry-run

    bosun apply --bundle ./bundle --tags=production --dry-run

Exit 0 — всё стоит как описано. Exit 2 — обнаружен drift (есть pending
changes). Exit 1/3/4 — настоящая ошибка.
