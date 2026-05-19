# nginx-demo

Минимальный bundle bosun-client'а в формате rev 2: ставит nginx и рендерит
`/etc/nginx/nginx.conf` из шаблона. Демонстрирует role-based layout,
inventory.read + inventory.merge и module-relative template().

## Запуск в Docker

Соберите бинарь bosun под bookworm-slim glibc:

    make bosun-bookworm

Постройте образ-сборку:

    docker build -t bosun-test-base:latest -f docker/test-base.Dockerfile .

Запустите контейнер и примените bundle:

    docker run --rm -v $(pwd):/work bosun-test-base:latest bash -c '
        cp /work/target/bookworm-release/bosun /usr/local/bin/bosun
        bosun apply --bundle /work/examples/nginx-demo/bundle \
          --tags=production \
          --lock-path /tmp/bosun.lock \
          --state-dir /tmp/bosun-state \
          --log-dir /tmp/bosun-log \
          --backup-dir /tmp/bosun-backups \
          --metric-file /tmp/bosun.prom
        dpkg-query -W nginx
        cat /etc/nginx/nginx.conf
    '

## Структура

```
bundle/
├── bundle.toml                       # метаданные + bundle.tags + bundle.inventory
├── manifests/main.star               # entry: загружает inventory + роль nginx
├── inventory/
│   ├── base.yaml                     # дефолты
│   ├── production.yaml               # overrides для tags=production
│   └── staging.yaml                  # overrides для tags=staging
└── roles/nginx/
    ├── main.star                     # def configure(inv): apt + file.content
    └── templates/nginx.conf.j2       # шаблон рядом с ролью
```

`bundle.toml` теперь содержит `[bundle.inventory] default_merge_strategy` и
`[bundle.tags]` — документация активных тэгов для `--help`.

## Тэги

Манифест вызывает `tags.require_one_of("production", "staging")` — без флага
`--tags` или с неизвестным значением CLI вернёт exit 3. Активные тэги
сортируются и пишутся в `bosun_tags.prom` (рядом с `bosun.prom`).

## Валидация без apply

    bosun bundle validate --bundle examples/nginx-demo/bundle --tags=production

Печатает `evaluate OK, N resources registered` или диагностику с exit 3.

## dry-run

    bosun apply --bundle examples/nginx-demo/bundle --tags=production --dry-run ...

Exit-code 2 — есть drift, 0 — всё в нужном состоянии.
