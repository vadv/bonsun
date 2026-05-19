# bosun-client

Rust SCM-агент, применяющий декларативный bundle на Starlark на ноде.
Запускается под root по таймеру (runr/cron/systemd), читает bundle с
манифестами и шаблонами, собирает факты о ноде, считает план и применяет
ресурсы по топологическому порядку с advisory-lock'ом и
Prometheus-метрикой для node_exporter.

## Возможности MVP

- Установка пакетов: `apt.package` с пином версии и восстановлением из
  half-configured dpkg-state.
- Управление файлами: `file.content` + `template()` на minijinja с
  атомарной заменой через `tempfile + rename`, backup-ротацией и
  rejection symlink'ов.
- Сбор фактов о ноде: hostname, init-system, cgroup-aware cpu/memory,
  is_pod, dpkg-state. Каждый факт имеет явное состояние `Known/Unknown/Stale`,
  которое уходит в метрики с per-fact-name label'ом — без этого на 60k
  нод нельзя вычленить «у X тысяч нод dpkg сломан».
- Lazy dirty-tracking фактов: `apt.package` помечает `installed_packages`
  устаревшим до apply, чтобы следующий `get` пересобрал dpkg-state.
- Метрика прогона в Prometheus textfile-collector с per-resource
  outcome'ами и per-fact state'ами.

## Документы

- Спецификация: `../docs/superpowers/specs/2026-05-18-bosun-client-mvp-design.md`
- План имплементации: `../docs/superpowers/plans/2026-05-18-bosun-client-mvp.md`
- Пример bundle'а: [`examples/nginx-demo/README.md`](examples/nginx-demo/README.md)

## Структура крейтов

- `crates/bosun-core` — типы (`Resource`, `ResourceId`, `Diff`, `Registry`),
  контракты примитивов (`Primitive`, `FactsSource`), Starlark-evaluator,
  Bundle-loader, Orchestrator.
- `crates/bosun-facts` — коллекторы фактов с trait'ом `Fact` и
  failure-mode `Known/Unknown/Stale`.
- `crates/bosun-primitives` — реализации `apt.package`, `file.content`,
  `template()`.
- `crates/bosun-cli` — бинарь `bosun`, парсинг аргументов, оркестровка
  apply/dry-run, BDD-тесты в реальном Docker.

## Быстрый старт

```bash
make build               # cargo build --release
make test                # cargo test --workspace --lib --bins --tests
make fmt                 # cargo fmt --all -- --check
make clippy              # workspace clippy с deny-warnings
make test-bdd            # BDD в Docker (требует docker)
make test-bdd TAGS=@apt-package    # только @apt-package сценарии
```

## Статический musl-бинарь

Для деплоя на ноду собирается один самодостаточный бинарь под musl —
без зависимостей от системных glibc, openssl, libdbus, libpq. Установка
на target: `scp` + `chmod +x`, никакие пакеты ставить не нужно.

```bash
make musl-x86_64      # статический бинарь под x86_64 (нужен musl-tools)
make musl-aarch64     # под aarch64 (нужен aarch64-linux-musl-gcc)
make musl-docker      # воспроизводимая сборка в rust:alpine, без host-toolchain
make musl-verify      # smoke-test в distroless/static контейнере
```

Результат — `target/x86_64-unknown-linux-musl/release/bosun`, ~25 MB,
`ldd` показывает `statically linked`. Запускается на любом Linux с
ядром ≥ 3.2, в том числе на distroless/static и scratch.

## Принципы

- Минимум exec, минимум флюктуаций состояния.
- Никаких паник в production-path. Все ошибки — через `Result`, все
  inv-ключи проверены явно через `fail()` в Starlark.
- Failure-mode фактов явная — никаких silent-fallback'ов с дефолтами
  под видом реальных данных.
- Полнота тестирования: unit + golden + BDD в реальном Docker без моков
  apt/dpkg/FS.

Подробнее — см. спецификацию.
