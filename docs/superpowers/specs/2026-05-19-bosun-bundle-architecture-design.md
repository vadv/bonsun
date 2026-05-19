---
title: "bosun bundle architecture — Design"
date: 2026-05-19
status: draft (rev 2, pending review)
author: dmitrivasilyev
supersedes_sections:
  - "Bundle формат" (из docs/superpowers/specs/2026-05-18-bosun-client-mvp-design.md, секции на manifests/main.star + defaults/main.yaml + templates/*.j2)
related:
  - docs/superpowers/research/2026-05-19-bundle-architecture-research.md
  - .claude-memory-compiler/knowledge/concepts/bundle-architecture.md
  - .claude-memory-compiler/knowledge/concepts/runr-supervisor.md
---

# bosun bundle architecture — Design

## Контекст и цель

Текущий bundle в MVP — один `manifests/main.star`, один `defaults/main.yaml`, плоский `templates/*.j2`. Это работает для демо с одним nginx-ресурсом. Реальный парк имеет много ролей (`postgres-chiit` — 16 ролей, 94 Go-файла, 37 шаблонов, 25 иерархических inventory yaml). Текущий формат не масштабируется и не дает явного разделения по ролям.

Эта итерация переделывает формат bundle под реальные потребности парка:

- **Role-based directory layout.** Каждая роль — отдельная директория с собственными templates.
- **Shared library mechanism.** Переиспользуемые helpers и template-рендереры выносятся в `_lib/<name>/` (например, `_lib/runr/` рендерит unit-файлы runr).
- **Explicit inventory loading.** Никакого auto-scan defaults; main.star явно перечисляет inventory-файлы и порядок их слияния через `inventory.load` + `inventory.merge`.
- **Tags-based selective loading.** CLI принимает `--tags=production,canary`; манифест ветвится по `tags.has()` и обязан вызвать `tags.require_one_of(...)` в начале, иначе fail-fast.
- **Module-relative template resolution.** `template("foo.j2")` внутри роли резолвится в `roles/<role>/templates/foo.j2`; внутри `_lib/runr/main.star` — в `_lib/runr/templates/foo.j2`. Cross-module template-access запрещён.

Distribution (tar.gz, signing, HTTPS bosun-server, cosign, reproducible build) — **out of scope**. Bundle — это директория. CLI принимает её путь.

## Принципы

1. **Явное лучше неявного.** Никакого auto-merge defaults/*.yaml, никакого implicit ordering. Сама структура манифеста описывает порядок.
2. **Изоляция через файловые границы.** Роль видит только свои templates. Lib — то же самое. Чтобы поделиться кодом, его выносят в `_lib/<name>/`.
3. **Минимальный API.** Inventory, tags, template — несколько чётких функций. Никаких 22-уровневых precedence hierarchies (Ansible), никаких 15-tier attribute scopes (Chef).
4. **Out-of-scope честно out-of-scope.** Не делаем подпись/cosign/MANIFEST/sha256/cache/reproducible build в этой итерации. Все они зафиксированы как future work с явным заголовком.

## Layout

```
mybundle/
├── bundle.toml                   # метаданные (см. ниже)
├── manifests/
│   └── main.star                 # entry point; CLI ссылается на него через bundle.toml.entry
├── inventory/                    # глобальный инвентарь; имена и подкаталоги — на усмотрение автора
│   ├── base.yaml
│   ├── production.yaml
│   ├── staging.yaml
│   └── postgresql/
│       ├── 01_packages.yaml
│       ├── 02_install.yaml
│       └── ...
├── roles/
│   ├── postgres/
│   │   ├── main.star             # экспортирует функции (configure, install, ...)
│   │   └── templates/            # ТОЛЬКО для этой роли
│   │       ├── postgresql.conf.j2
│   │       └── pg_hba.conf.j2
│   ├── patroni/
│   │   ├── main.star
│   │   └── templates/
│   │       └── patroni.yml.j2
│   └── ...
└── _lib/                         # опционально: shared helpers
    ├── runr/
    │   ├── main.star             # render_service(...), render_timer(...)
    │   └── templates/
    │       ├── service.j2
    │       ├── timer.j2
    │       └── cgroup.j2
    └── apt/
        └── main.star             # add_repository(name, uri, key), без своих templates
```

Несколько правил:

- `bundle.toml` обязателен и лежит в корне.
- `manifests/main.star` — единственный entry point. Внутри загружает inventory и роли через `load`.
- `inventory/` — произвольная иерархия yaml-файлов. **Структура не имеет значения для bosun-core**: загрузку определяет автор bundle через `inventory.load("inventory/<path>")`.
- `roles/<role>/main.star` обязателен для каждой роли. `roles/<role>/templates/` опционален.
- `_lib/<name>/main.star` обязателен для каждого lib-модуля. `_lib/<name>/templates/` опционален.
- `_lib` — это **именованная директория**, а не префикс. Подкаталоги её не «_lib`-prefix», это `_lib/<name>/`. Это маркирует «не-роль» и одновременно даёт пространство имён для load.
- Несоответствие имени директории (`_lib`) и load-namespace (`@lib/`) — намеренное. `_` в имени директории сигнализирует «не сканируйте как роль», `@lib/` — короткий и читаемый префикс импорта.
- Никакие другие top-level директории не имеют семантического значения; bosun их игнорирует. Авторы могут класть `README.md`, `CHANGELOG.md`, `.gitignore`, тесты, что угодно.

## `bundle.toml` schema

```toml
[bundle]
name           = "postgres-cluster"   # required, lowercase-with-dashes, [a-z0-9-]+
version        = "1.4.0"              # required, semver
description    = "Postgres + Patroni + pgbouncer fleet bundle"   # optional
requires_bosun = "^0.4"               # required, semver-req для bosun-core
entry          = "manifests/main.star"   # required, путь относительно корня bundle

[bundle.inventory]
default_merge_strategy = "deep_map_replace_list"   # optional; используется когда inventory.merge вызван без strategy=

[bundle.tags]
production = "Production cluster, real workload"
staging    = "Staging cluster"
canary     = "Subset of production for testing risky changes"
```

Что **намеренно отсутствует**:

- `[[bundle.roles]]` — нет, роли не имеют отдельной версии или дистрибуции; они часть bundle.
- `bundle.lock` — нет, всё внутри bundle, lock-файл не нужен.
- `bundle.signing` — out of scope.
- `MANIFEST` — out of scope; debug-friendly directory format.
- `bundle.conflicts_with` — out of scope.

Парсер: `serde::Deserialize` из toml в `struct BundleMetadata { name, version, description: Option<String>, requires_bosun, entry, tags: BTreeMap<String, String>, inventory: BundleInventoryConfig }`. Все строки должны быть валидным UTF-8. `[bundle.tags]` — документация для CLI `--help`, во время evaluate не валидируется.

### `requires_bosun` — точная semver-семантика

Проверка совместимости — `semver::VersionReq::matches(&Version::parse(env!("CARGO_PKG_VERSION"))?)`. Каретный синтаксис соответствует Cargo semver:

- `^0.4` для 0.x → `>=0.4.0, <0.5.0`. Bundle сломается при выходе bosun 0.5.
- `^1.4` для 1.x → `>=1.4.0, <2.0.0`. Bundle переживёт минорные апгрейды до 2.0.

Оперативное правило: для bundle, рассчитанных пережить минорные апгрейды агента, используйте `>=0.4` (не каретный) вместо `^0.4`. Для bundle, жёстко привязанных к конкретному минору bosun (например, использует свежий primitive), оставляйте `^0.4`.

Поведение `requires_bosun` относительно MVP не меняется — это только формализация контракта, который уже есть.

### `entry` — валидация пути

`entry` читается как `String`, но к нему применяются те же path-safety правила, что и к остальным резолверам: путь обязан быть относительным, без `..` сегментов, без NUL-байтов; после `canonicalize()` обязан начинаться с canonical bundle root. Проверка выполняется в `Bundle::load_dir` до любой дальнейшей обработки (см. секцию «Path safety helper» и «Безопасность» ниже).

## Loading flow

```
bosun apply --bundle ./mybundle/ --tags=production
```

1. CLI парсит `--bundle` (PathBuf), `--tags` (CSV → Vec<String>). После парсинга CLI дедуплицирует и сортирует tags для детерминизма.
2. `Bundle::load_dir(path)`:
   - Читает `bundle.toml`.
   - Проверяет `requires_bosun` против `env!("CARGO_PKG_VERSION")` через semver.
   - Валидирует `bundle.toml.entry` через path-safety helper (см. ниже).
   - НЕ сканирует inventory/ и roles/ заранее — только при evaluate, по запросу из Starlark.
3. Создаёт `Evaluator` с `EvaluatorConfig` (см. ниже).
4. Запускает starlark eval `manifests/main.star`:
   - `load("@bosun/builtins", ...)` → встроенные globals.
   - `load("@roles/<name>", ...)` → грузит `roles/<name>/main.star`.
   - `load("@lib/<name>", ...)` → грузит `_lib/<name>/main.star`.
   - `inventory.load("inventory/<path>")` → читает yaml в Starlark Value.
   - `tags.has("...")`, `tags.require_one_of(...)` — runtime gate.
5. Каждый вызов `apt.package(...)` / `file.content(...)` / `runr.service(...)` регистрирует Resource в Registry.
6. После eval — топ-сорт + apply per spec MVP.

## Starlark API расширения

### `load`-резолвер

Поддерживаемые пути:

| Префикс | Резолвится в | Видимость |
|---|---|---|
| `@bosun/builtins` | virtual module с native-globals (apt, file, runr, template, inventory, tags, ...) | везде |
| `@roles/<name>` | `<bundle_root>/roles/<name>/main.star` | везде |
| `@lib/<name>` | `<bundle_root>/_lib/<name>/main.star` | везде |
| `:<symbol>` или относительный путь внутри одного модуля | пока запрещено | — |

Все три префикса резолвятся через custom `starlark::eval::FileLoader` импл, который читает соответствующий .star файл, парсит, кеширует frozen Module.

**Циклические импорты** — детектируются на этапе load (Starlark сам ловит), возвращают `StarlarkGlueError::LoadCycle`. Композиция `_lib` → `_lib` поддерживается: `_lib/foo/main.star` имеет право `load("@lib/bar", ...)`. Цикл `_lib/foo` → `_lib/bar` → `_lib/foo` падает на parse-этапе со стандартной Starlark диагностикой, мы маппим её в `LoadCycle`.

**Privacy convention (Bazel-стиль).** Символы, начинающиеся с `_`, приватны для модуля; внешние `load` не могут их импортировать.

Проверка выполняется **не на стороне FileLoader** (FileLoader получает только путь модуля, не список запрашиваемых символов), а после evaluate-фазы при сборке frozen Module:

1. `FileLoader::load(path)` читает .star, парсит AstModule, создаёт временный `Module`, evaluate'ит его.
2. До `freeze()` glue-слой перечисляет `module.names()`.
3. Если starlark-rust 0.13 НЕ фильтрует `_`-имена при `load(...)` нативно — создаём новый `Module` («public view»), переносим в него только non-private символы через `module.set(name, value)`, freeze'им именно его.
4. Если starlark-rust 0.13 фильтрует автоматически (нужно проверить при имплементации против актуального API крейта) — оставляем frozen Module как есть, документируем зависимость.

Реализатору: выбрать explicit module-filter подход как переносимый между минорными версиями starlark-rust. Если 0.13 уже даёт встроенный механизм — переключиться, оставив комментарий о версии.

Если внешний модуль пытается импортировать `_private` — ошибка `BundleError::PrivateSymbol { symbol, module }`, exit 3.

### Module path resolution и кеш

`Bundle::resolve_module(load_path)` всегда возвращает **канонический абсолютный** PathBuf:

```rust
pub fn resolve_module(&self, load_path: &str) -> Result<PathBuf, BundleError> {
    let raw = self.root.join(match load_path {
        p if p.starts_with("@roles/") => format!("roles/{}/main.star", &p["@roles/".len()..]),
        p if p.starts_with("@lib/")   => format!("_lib/{}/main.star",  &p["@lib/".len()..]),
        _ => return Err(BundleError::UnsupportedLoadPath { load_path: load_path.into() }),
    });
    path_safety::resolve_within_root(&self.root, raw.strip_prefix(&self.root)?)
}
```

Канонизация выполняется один раз; symlink-bundle root резолвится единственный раз.

`BundleLoader` кеш — `RefCell<HashMap<PathBuf, FrozenModule>>` с canonical PathBuf в качестве ключа. Кеш живёт ровно один вызов `evaluate_manifest`; между вызовами Evaluator создаётся свежий BundleLoader.

### `inventory` module

```python
load("@bosun/builtins", "inventory")

# Загружает yaml-файл по пути относительно корня bundle.
# Возвращает Starlark dict (map с любой вложенностью).
inv = inventory.load("inventory/base.yaml")

# Сливает несколько inventory-источников. Strategy указывает поведение
# при коллизиях ключей и для list-полей. Если strategy опущен,
# берётся bundle.toml -> [bundle.inventory].default_merge_strategy.
merged = inventory.merge(
    inv1, inv2, inv3,
    strategy = "deep_map_replace_list",
)

# Альтернатива для list-of-records, объединяемых по ключевому полю.
merged_servers = inventory.merge_keyed(s1, s2, key = "id")
```

#### `inventory.load(path: str) -> dict`

- `path` — относительный путь от корня bundle. Должен проходить тот же `path_safety::resolve_within_root` helper, что и `entry` / `resolve_module` / `template()`: не абсолютный, без `..`, без NUL, после canonicalize начинается с bundle root, не symlink.
- Читает yaml через `serde_norway` (preserves insertion order для maps — это используется для детерминизма при последующем merge), парсит в `serde_json::Value`, конвертирует в Starlark Value (Object → Dict, Array → List, scalars — как есть).
- При ошибке (файл не найден, синтаксис) — `fail("inventory: read 'inventory/...': <reason>")`.
- **Кешируется per-`evaluate_manifest`.** Канонический путь — ключ кеша. Второй вызов `inventory.load` с тем же путём возвращает закешированный распарсенный Value, файл повторно не читается. Между разными invocations Evaluator'а кеш сбрасывается (новый BundleLoader = новый кеш). Соглашение симметрично кешу FileLoader: оба keyed by canonical PathBuf, оба per-evaluate.

#### `inventory.merge(*sources: dict, strategy: str = <default>) -> dict`

Если `strategy` не указан:
1. Берётся `bundle.toml -> [bundle.inventory].default_merge_strategy`.
2. Если и там не указан — `fail("inventory.merge: missing default merge strategy; set [bundle.inventory].default_merge_strategy in bundle.toml or pass strategy= argument")`.

Стратегии:

| `strategy` | Поведение для map | Поведение для list | Поведение для scalar |
|---|---|---|---|
| `"deep_map_replace_list"` | deep merge по ключам; правый источник побеждает | правый источник заменяет полностью | правый побеждает |
| `"deep_map_append_list"` | deep merge | concat правый к левому, дедупликация по equality | правый побеждает |
| `"replace"` | правый источник заменяет полностью | правый источник заменяет полностью | правый побеждает |

`null` в правом источнике на любом уровне → удаляет ключ из левого.

#### `inventory.merge_keyed(*sources, key: str) -> dict`

Отдельная функция (вместо `strategy="deep_map_keyed_list:<key>"` — это плохо читается и плохо валидируется). Поведение:

- Top-level — deep merge как `deep_map_replace_list`.
- Любой list внутри top-level — обязан состоять из maps. Элементы объединяются по совпадению значения `<key>`. Совпавшие — deep-merge, новые — добавляются в конец, порядок левого источника сохраняется.
- Если элемент list не map или у него нет `<key>` — `fail("inventory.merge_keyed: list at <path> contains element without key '<key>'")`.

Реализация:

```rust
pub enum MergeStrategy {
    DeepMapReplaceList,
    DeepMapAppendList,
    Replace,
}

pub fn merge_inventory(
    base: serde_json::Value,
    over: serde_json::Value,
    strategy: &MergeStrategy,
) -> serde_json::Value;

pub fn merge_inventory_keyed(
    base: serde_json::Value,
    over: serde_json::Value,
    key: &str,
) -> Result<serde_json::Value, InventoryError>;
```

Тесты: для каждой стратегии — happy path + edge case (collision на разных уровнях глубины, null deletion, type mismatch обработка). Для `merge_keyed` — happy path, дубликат key в одном источнике, list-of-non-maps, missing key.

#### Starlark-signature

```rust
#[starlark_module]
fn inventory_globals(builder: &mut GlobalsBuilder) {
    fn r#load(path: String) -> anyhow::Result<starlark::values::Value<'v>> { /* ... */ }

    fn merge<'v>(
        #[starlark(args)] args: starlark::values::tuple::UnpackTuple<starlark::values::Value<'v>>,
        #[starlark(default = "")] strategy: String,
    ) -> anyhow::Result<starlark::values::Value<'v>> { /* пустая строка → fall back на default из bundle.toml */ }

    fn merge_keyed<'v>(
        #[starlark(args)] args: starlark::values::tuple::UnpackTuple<starlark::values::Value<'v>>,
        key: String,
    ) -> anyhow::Result<starlark::values::Value<'v>> { /* ... */ }
}
```

Шаблон взят из `starlark-0.13.0/src/stdlib/funcs/other.rs` (variadic args через `UnpackTuple`).

### `tags` module

```python
load("@bosun/builtins", "tags")

# Возвращает True если tag активен (был в --tags на CLI).
tags.has("production")  # bool

# Гарантирует что хотя бы один из аргументов активен. Иначе fail.
tags.require_one_of("production", "staging", "canary")
# raises fail("tags: expected one of [production, staging, canary] in active set, got []")

# Возвращает текущий активный набор для логирования.
tags.active()  # list[str], lexically sorted for determinism
```

#### Поведение

- Активный набор = `HashSet<String>` от `--tags=` (CSV). CLI дедуплицирует и сортирует на входе в Evaluator.
- Если CLI не задал `--tags`, набор пустой.
- `tags.has(x)` — `x in active_set`.
- `tags.require_one_of(...)` — если пересечение с активным пустое, `fail()` с сообщением включающим перечень ожидаемых и фактический набор.
- `tags.active()` — отсортированная копия списка.

#### Когда вызывать `require_one_of`

Рекомендация в документации (НЕ enforce-able в Rust): в начале `main.star` сразу после `load`-блоков. Это даёт fail-fast. Тэги вне `[bundle.tags]` в bundle.toml — допускаются (CLI может передать что угодно), но author bundle обычно валидирует через `require_one_of`.

#### Observability

Активный набор тэгов виден в двух местах:

1. **Prometheus textfile.** Для каждого активного тэга агент пишет gauge `bosun_active_tags{tag="<name>"} 1`. При следующем запуске старые записи перезаписываются; неактивные тэги исчезают. Метрика расширяет существующий набор bosun-метрик (см. секцию «Метрики» MVP-спека).
2. **Логи.** При старте evaluate агент пишет `tracing::info!("bosun: active tags = {:?}", sorted_tags)`. Видно через `journalctl -u bosun` / stdout.

### `template(path: str) -> str` — module-relative

В MVP `template()` искал шаблон в `<bundle>/templates/<path>`. В новой архитектуре — резолвится **относительно того модуля, чья функция в данный момент выполняется**:

- Внутри функции, определённой в `roles/postgres/main.star` → `<bundle>/roles/postgres/templates/<path>`.
- Внутри функции, определённой в `_lib/runr/main.star` → `<bundle>/_lib/runr/templates/<path>`.
- Прямой вызов из `manifests/main.star` (top-level или функция, определённая в этом же файле) — **запрещён**. `manifests/main.star` — это orchestration, не config content. Author кладёт template в роль или lib, оттуда зовёт `template()`. Реакция: `fail("template: cannot be called from manifests/main.star; move rendering into a role or @lib module")`.

Это соответствует UX-принципу: «пользователь должен видеть всё так, будто он находится в директории своего файла». `template("foo.j2")` рядом с .star-файлом, который содержит функцию — резолвится в `templates/` той же директории.

Cross-module access (`template("@roles/postgres:foo.j2")`) — **запрещено**. Никаких префиксов в `path`. Reject с `fail("template: cross-module access forbidden, path must be relative to the calling module")`.

#### Механизм: walk call stack до user-defined frame

`template(path, eval)` — native function, имеет доступ к `Evaluator`. Из него получает call stack и идёт сверху (от текущего фрейма) вниз, пропуская native frames, до первого user-defined фрейма. Имя файла из его `codemap` — это путь к .star файлу, в котором определена функция, чьё тело сейчас исполняется.

Псевдокод (точные имена API уточняются при имплементации против starlark-rust 0.13):

```rust
fn template_native(path: String, eval: &mut Evaluator) -> anyhow::Result<String> {
    let call_stack = eval.call_stack();
    let defining_file = call_stack
        .frames()
        .iter()
        .rev()
        .find_map(|frame| frame.codemap().map(|cm| cm.file_name().to_path_buf()))
        .ok_or_else(|| anyhow::anyhow!("template: no user frame on call stack"))?;

    // Канонический путь .star файла — даём resolve_template.
    let bundle: &Bundle = eval.extra::<EvalState>()?.bundle.as_ref();
    let template_path = bundle.resolve_template(&defining_file, &path)?;
    render_template(&template_path)
}
```

Конкретный API крейта starlark-0.13 (например, `Evaluator::call_stack()` → `CallStack`, `Frame::location()` → `Option<FrameSpan>`, `FrameSpan::file()` → `&CodeMap`) уточняется реализатором — нужен walk без allocation на горячем пути. См. crate-docs `starlark::eval::CallStack` и `starlark::codemap::CodeMap` для starlark-rust 0.13.

`BundleLoader` при load парсит .star уже с canonical bundle-relative путём в `AstModule::parse`, поэтому имя файла, которое отдаёт codemap, — каноническое.

**Fallback (если 0.13 не даёт доступ к codemap из call stack).** Альтернатива — bind фрешного `template`-callable в globals каждого Module при load:

- `BundleLoader::load(path)` создаёт closure `|t| resolve_template_from(bundle_root, defining_module=path, t)`.
- Вставляет в `Module::set("template", ...)` перед evaluate.
- Тот же `template` имя, но каждый Module видит свой Value, baked-in module path.
- Минус: `template` не передаётся между модулями как обычное значение. Это OK — её и не передают, её зовут по имени.

**Решение:** имплементация выбирает call-stack-walk как primary; если в процессе обнаруживается, что 0.13 API не позволяет — переключается на per-Module bound closure. Спека фиксирует один из двух (тот, который реализован), не оба. Решение протоколируется отдельной записью в `decisions/` после имплементации.

#### `resolve_template` — детали

`Bundle::resolve_template(module: &Path, template_rel: &str)`:

- `module` — канонический путь .star файла.
- Если `module = <root>/roles/<name>/main.star` → resolve к `<root>/roles/<name>/templates/<template_rel>`.
- Если `module = <root>/_lib/<name>/main.star` → resolve к `<root>/_lib/<name>/templates/<template_rel>`.
- Если `module = <root>/manifests/main.star` → `BundleError::TemplateFromManifests` (см. выше про запрет).
- Иначе — `BundleError::UnsupportedModuleForTemplate`.
- После resolve — те же проверки безопасности что в MVP F01 через общий `path_safety::resolve_within_root`: `is_absolute=false`, no `..`, no NUL, canonicalize + starts_with check, reject symlinks.
- Если у роли/lib нет директории `templates/` или нет файла — `FileNotFound { hint: "create roles/<name>/templates/<file> or remove the template() call" }`. Это **OK кейс** для lib без templates — пока никто не зовёт `template()`, директория не нужна.

## Path safety helper

Единый helper в `bosun-core` используется четырьмя резолверами: `bundle.toml.entry`, `inventory.load`, `resolve_module` (для `@roles/`/`@lib/`), `resolve_template`.

```rust
// bosun-core/src/path_safety.rs
pub fn resolve_within_root(
    root: &Path,
    relative: &str,
) -> Result<PathBuf, PathSafetyError>;

#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PathSafetyError {
    #[error("path must be relative, got absolute: {0}")]
    Absolute(String),
    #[error("path contains parent-dir segment ('..'): {0}")]
    ParentDir(String),
    #[error("path contains NUL byte")]
    NulByte,
    #[error("path not found: {0}")]
    NotFound(PathBuf),
    #[error("path resolves outside root: attempted={attempted:?}, root={root:?}")]
    NotInRoot { root: PathBuf, attempted: PathBuf },
    #[error("path is a symlink (refusing to follow): {0}")]
    IsSymlink(PathBuf),
}
```

Поведение:

1. Reject если `relative` начинается с `/` или содержит `..` сегмент или NUL.
2. Join с `root`, `canonicalize()`. NotFound → `PathSafetyError::NotFound`.
3. Reject если результат не начинается с canonical root.
4. `symlink_metadata` — если тип = symlink → `PathSafetyError::IsSymlink`.
5. Возвращает канонический PathBuf.

Все четыре сайта вызывают эту функцию. Никаких локальных копий проверок. Это центральная точка path-safety; security audit проверяет ровно один helper.

## Изменения в bosun-core

### `Bundle` struct

Текущая:
```rust
pub struct Bundle {
    pub metadata: BundleMetadata,
    pub root: std::path::PathBuf,
    pub manifests: std::collections::HashMap<std::path::PathBuf, String>,  // relative → contents
    pub templates_root: std::path::PathBuf,
    pub defaults: serde_json::Value,
}
```

Новая:
```rust
pub struct Bundle {
    pub metadata: BundleMetadata,
    pub root: std::path::PathBuf,                   // canonical
    pub entry: std::path::PathBuf,                  // canonical absolute path, validated через path_safety
    // Manifests/roles/lib загружаются on-demand FileLoader'ом, не предзагружаются в HashMap.
}

impl Bundle {
    pub fn load_dir(path: &std::path::Path) -> Result<Self, BundleError>;
    pub fn check_compatibility(&self, current_version: &str) -> Result<(), BundleError>;
    pub fn resolve_module(&self, load_path: &str) -> Result<PathBuf, BundleError>;
    pub fn resolve_template(&self, module: &Path, template_rel: &str) -> Result<PathBuf, BundleError>;
}
```

### `BundleError` enum

Расширяется (`#[non_exhaustive]`):

```rust
#[non_exhaustive]
pub enum BundleError {
    Io { path: String, source: std::io::Error },
    InvalidManifest(String),
    EntryNotFound(String),
    InvalidYaml { path: String, source: serde_norway::Error },
    VersionIncompatible { required: String, current: String },
    ModuleNotFound { load_path: String, fs_path: PathBuf },          // NEW
    UnsupportedLoadPath { load_path: String },                        // NEW
    UnsupportedModuleForTemplate { module: PathBuf },                // NEW
    TemplateFromManifests { hint: String },                           // NEW
    PrivateSymbol { symbol: String, module: PathBuf },                // NEW
    PathSafety(#[from] PathSafetyError),                              // NEW
    DefaultMergeStrategyMissing,                                      // NEW
}
```

### `starlark_glue::load_resolver`

Текущий обрабатывает только `@bosun/builtins`. Новый поддерживает три префикса, privacy enforcement и канонический cache key.

```rust
impl<'a> FileLoader for BundleLoader<'a> {
    fn load(&self, load_path: &str) -> anyhow::Result<FrozenModule> {
        match load_path {
            "@bosun/builtins" => Ok(self.builtins.clone()),
            p if p.starts_with("@roles/") || p.starts_with("@lib/") => self.load_role_or_lib(p),
            _ => Err(StarlarkGlueError::UnsupportedLoad { load_path: p.into() }.into()),
        }
    }
}

impl<'a> BundleLoader<'a> {
    fn load_role_or_lib(&self, load_path: &str) -> anyhow::Result<FrozenModule> {
        let resolved = self.bundle.resolve_module(load_path)?;   // canonical PathBuf
        if let Some(cached) = self.cache.borrow().get(&resolved) {
            return Ok(cached.clone());
        }

        let _guard = ModuleStackGuard::push(&self.state, resolved.clone());
        let frozen = self.parse_and_eval(&resolved)?;            // ? safe — guard pops on drop
        let public = self.filter_private_symbols(frozen)?;       // см. Privacy выше

        self.cache.borrow_mut().insert(resolved.clone(), public.clone());
        Ok(public)
    }
}
```

#### `ModuleStackGuard` — RAII для стека current_module

Mimics существующий `StateGuard` в `starlark_glue/mod.rs:97`.

```rust
struct ModuleStackGuard<'s> {
    state: &'s EvalState,
}

impl<'s> ModuleStackGuard<'s> {
    fn push(state: &'s EvalState, module: PathBuf) -> Self {
        state.current_module.borrow_mut().push(module);
        Self { state }
    }
}

impl Drop for ModuleStackGuard<'_> {
    fn drop(&mut self) {
        self.state.current_module.borrow_mut().pop();
    }
}
```

При panic во время eval — `_guard` дропается, стек консистентен. При `?`-пробрасывании ошибки — то же. Это критично: без RAII при ошибке `parse_and_eval` стек остался бы засорённым и следующий `template()` резолвил бы из неправильной директории.

Кеш — `RefCell<HashMap<PathBuf, FrozenModule>>` внутри `BundleLoader`, scoped per-`evaluate_manifest` invocation. Канонический путь как ключ — один и тот же symlinked bundle root резолвится единственный раз.

### `EvaluatorConfig` — single-config объект

Чтобы избежать 8+ позиционных параметров в `evaluate_manifest`, новые поля собраны в один config:

```rust
pub struct EvaluatorConfig {
    pub bundle: Rc<Bundle>,
    pub primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>,
    pub facts: Rc<dyn FactsSource>,
    pub tags: HashSet<String>,
    pub sensitive: Rc<SensitiveStore>,
    pub template_fn: TemplateFn,
    pub plan_ctx: PlanCtx,
}

pub fn evaluate_manifest(config: EvaluatorConfig) -> Result<Registry, EvaluatorError>;
```

Все 18 callsites в `starlark_glue/mod.rs` тестах + `bosun-cli/src/run.rs` мигрируют на конструкцию config-объекта. Refactor одноразовый, экономия — каждый новый параметр evaluate добавляется как поле, а не как новый аргумент во все callsites.

### `EvalState` — расширение

```rust
pub struct EvalState {
    // existing
    pub template_fn: TemplateFn,
    pub sensitive: Rc<SensitiveStore>,
    pub primitives: Rc<HashMap<ResourceKind, Box<dyn Primitive>>>,
    pub facts: Rc<dyn FactsSource>,
    pub plan_ctx: PlanCtx,
    pub registry: Rc<RefCell<Registry>>,

    // NEW
    pub bundle: Rc<Bundle>,                       // Rc, не Arc — single-thread evaluator
    pub tags: HashSet<String>,
    pub current_module: RefCell<Vec<PathBuf>>,
    pub inventory_cache: RefCell<HashMap<PathBuf, FrozenValue>>,
}
```

`Rc` (не `Arc`) — evaluator однопоточный, остальные shared-поля уже `Rc`, держим консистентно.

### `starlark_glue::globals` — расширение

```rust
// inventory
fn inventory_load(path: String) -> anyhow::Result<Value>;
fn inventory_merge(sources: Vec<Value>, strategy: String) -> anyhow::Result<Value>;
fn inventory_merge_keyed(sources: Vec<Value>, key: String) -> anyhow::Result<Value>;

// tags
fn tags_has(tag: String) -> bool;
fn tags_require_one_of(tags: Vec<String>) -> anyhow::Result<NoneType>;
fn tags_active() -> Vec<String>;

// template (уже есть, но module-relative — пересоздаётся под call-stack walk)
fn template(path: String, eval: &mut Evaluator) -> anyhow::Result<String>;
```

### Error chain в Starlark-glue

- `impl From<BundleError> for StarlarkGlueError` уже существует в mod.rs.
- `FileLoader::load` возвращает `anyhow::Result<FrozenModule>` (требование starlark-rust). Структурные ошибки оборачиваются через `anyhow::Error::from(StarlarkGlueError::...)`, что сохраняет `source()` цепь.
- Когда starlark eval поднимает ошибку, glue layer пытается downcast'ить anyhow Error в `StarlarkGlueError`/`BundleError` через `error.downcast_ref::<...>()` и маппит в `Error::ManifestEval { kind, ... }` со структурными полями.

### CLI

В `bosun-cli/src/args.rs`:

```rust
#[derive(Debug, clap::Args)]
pub struct ApplyArgs {
    #[arg(long)]
    pub bundle: PathBuf,

    // REMOVED in this iteration: --inventory (раньше был override-yaml).
    // Inventory полностью внутри bundle.

    /// CSV-list of active tags. CLI deduplicates and sorts before passing to evaluator.
    #[arg(long, value_delimiter = ',')]
    pub tags: Vec<String>,

    /// ... existing fields (dry_run, continue_on_error, log-level, ...)
}
```

`--inventory` удаляется. Если кому-то нужно подменить inventory локально — пусть редактирует файл `inventory/*.yaml` (bundle же directory).

В `bosun-cli/src/run.rs`:

```rust
let mut tags: Vec<String> = args.tags.iter().cloned().collect();
tags.sort_unstable();
tags.dedup();
let tags: HashSet<String> = tags.into_iter().collect();

tracing::info!(active_tags = ?{
    let mut v: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
    v.sort_unstable();
    v
}, "bosun: active tags");
write_active_tags_metric(&tags)?;   // /var/lib/node_exporter/textfile_collector/bosun.prom

let config = EvaluatorConfig {
    bundle: Rc::new(bundle),
    primitives: Rc::new(primitives),
    facts: Rc::new(facts_snapshot),
    tags,
    sensitive: Rc::new(sensitive_store),
    template_fn,
    plan_ctx,
};
let registry = evaluate_manifest(config)?;
```

### `bosun bundle validate` subcommand

Отдельная команда для статической валидации bundle без обращения к системе.

```
bosun bundle validate --bundle <dir> --tags=<csv> [--facts=fixtures/facts.json]
```

Поведение:

1. `Bundle::load_dir(--bundle)`.
2. Если `--facts` указан — читает JSON и десериализует в `FactsSnapshot`. Иначе — пустой snapshot (все факты — None/0/false defaults).
3. Создаёт Evaluator с переданными тэгами, синтетическим snapshot и моками primitive'ов (primitive'ы не выполняются, только регистрируют ресурсы).
4. Вызывает `evaluate_manifest`.
5. Печатает результат:
   - 0 — clean: «evaluate OK, N resources registered».
   - 3 — eval error: manifest syntax, inventory key missing, undefined Starlark symbol, tag mismatch, path-safety violation, и т.п. С полным diag-сообщением и stderr.

Применимо для CI bundle-репозиториев и локального fail-fast pre-commit.

Схема `fixtures/facts.json`:

```json
{
  "hostname": "test-node-01",
  "cpu_count": 8,
  "memory_mb": 16384,
  "is_pod": false,
  "init_system": "systemd",
  "installed_packages": {}
}
```

Поля — те же, что у `FactsSnapshot` (см. MVP-спек). Отсутствующие поля заполняются defaults.

## Безопасность

### Path safety

Все четыре резолвера (`entry`, `inventory.load`, `resolve_module`, `resolve_template`) проходят через `path_safety::resolve_within_root`. Reject правил наследуется из MVP F01 (relative-only, no parent-dir, no NUL, canonicalize + starts_with, reject symlinks). Это **обеспечивается одной функцией**, не четырьмя копиями — security audit проверяет один helper. Любые новые точки чтения файлов из bundle обязаны проходить через тот же helper.

### Trust boundary в directory mode

В этой итерации bundle — это директория, на которую указывает `--bundle <path>`. Trust boundary:

- **Directory at `<--bundle>` доверена как tamper-free.** Bosun читает `.star`, `.yaml`, `.j2` и пользуется их содержимым как «программа конфигурации». Любая запись внутрь bundle во время apply (например, side-process подменяет `roles/X/main.star`) ломает гарантии.
- **Production deployment.** Path должен быть mounted read-only, owned root:root, с ограниченными правами записи (0644 для файлов, 0755 для директорий). Никаких mid-apply writes от посторонних процессов.
- **Threat: hostile co-located process пишет в bundle directory.** Если такая угроза реальна — это уязвимость directory mode. Single mitigation в этой итерации — OS-уровневые controls (chown root, chmod, ro mount, AppArmor/SELinux).
- **Future: signed tar.gz distribution заменяет это cryptographic trust'ом** (sha256 + ed25519 signature; bosun отказывается грузить bundle без валидной подписи). Это вынесено в Distribution spec, см. Out-of-scope.

В текущей итерации bosun **не пытается** обнаружить tampering. Это контракт оператора.

## Пример: переписанный nginx-demo

`examples/nginx-demo/bundle/bundle.toml`:

```toml
[bundle]
name           = "nginx-demo"
version        = "0.2.0"
description    = "Minimal nginx bundle for bosun bundle-format tests"
requires_bosun = "^0.4"
entry          = "manifests/main.star"

[bundle.inventory]
default_merge_strategy = "deep_map_replace_list"

[bundle.tags]
production = "Production"
staging    = "Staging"
```

`examples/nginx-demo/bundle/inventory/base.yaml`:
```yaml
nginx_version: 1.22.1-9
worker_processes: auto
worker_connections: 768
```

`examples/nginx-demo/bundle/inventory/production.yaml`:
```yaml
worker_processes: 8
worker_connections: 4096
```

`examples/nginx-demo/bundle/inventory/staging.yaml`:
```yaml
worker_processes: 2
worker_connections: 256
```

`examples/nginx-demo/bundle/manifests/main.star`:

```python
load("@bosun/builtins", "inventory", "tags")
load("@roles/nginx", configure_nginx = "configure")

tags.require_one_of("production", "staging")

inv = inventory.load("inventory/base.yaml")
if tags.has("production"):
    inv = inventory.merge(inv, inventory.load("inventory/production.yaml"))
elif tags.has("staging"):
    inv = inventory.merge(inv, inventory.load("inventory/staging.yaml"))

configure_nginx(inv = inv)
```

`examples/nginx-demo/bundle/roles/nginx/main.star`:

```python
load("@bosun/builtins", "apt", "file", "template")

def configure(inv):
    apt.package(name = "nginx", version = inv["nginx_version"])

    file.content(
        path     = "/etc/nginx/nginx.conf",
        contents = template("nginx.conf.j2"),
        mode     = 0o644,
        owner    = "root",
        group    = "root",
    )
```

`examples/nginx-demo/bundle/roles/nginx/templates/nginx.conf.j2`:

```jinja
worker_processes {{ inv.worker_processes }};
events { worker_connections {{ inv.worker_connections }}; }
http { server { listen 80 default_server; } }
```

Запуск:
```
bosun apply --bundle examples/nginx-demo/bundle --tags=production
```

Что произойдёт:
1. `Bundle::load_dir(examples/nginx-demo/bundle)` читает `bundle.toml`, валидирует `entry` через path_safety.
2. CLI дедуплицирует, сортирует, формирует `tags = {"production"}`, пишет Prometheus textfile, лог.
3. Evaluator стартует с `manifests/main.star`.
4. `load("@bosun/builtins", "inventory", "tags")` → встроенные globals.
5. `load("@roles/nginx", configure_nginx = "configure")` → resolve через `Bundle::resolve_module` (canonical PathBuf), eval `roles/nginx/main.star` под ModuleStackGuard, экспортирует `configure` как `configure_nginx`.
6. `tags.require_one_of("production", "staging")` → OK (production active).
7. `inventory.load("inventory/base.yaml")` → reads yaml, кеширует.
8. `tags.has("production")` → True → load `inventory/production.yaml`, merge с дефолтной стратегией из bundle.toml.
9. `configure_nginx(inv = inv)` — выполняется функция, определённая в `roles/nginx/main.star`; вызов `template("nginx.conf.j2")` идёт по call stack, находит defining file = `roles/nginx/main.star`, резолвит к `roles/nginx/templates/nginx.conf.j2`.
10. Внутри роли `apt.package(...)` регистрирует Resource. `template()` отдаёт rendered string.
11. После eval — топ-сорт + apply.

## Идиомы

### Tag-as-filter-inside-role

Не следует превращать `main.star` в 200-строчный if/elif tree по тэгам. Главный файл должен быть плоским orchestration — какие роли подключаются, в каком порядке. Ветвление по среде — внутри роли.

```python
# manifests/main.star
load("@bosun/builtins", "inventory", "tags")
load("@roles/postgres", configure_postgres = "configure")
load("@roles/patroni",  configure_patroni  = "configure")

tags.require_one_of("production", "staging")

inv = inventory.merge(
    inventory.load("inventory/base.yaml"),
    inventory.load("inventory/production.yaml") if tags.has("production") else inventory.load("inventory/staging.yaml"),
)
configure_postgres(inv)
configure_patroni(inv)
```

```python
# roles/postgres/main.star
load("@bosun/builtins", "apt", "file", "service", "tags", "template")

def configure(inv):
    apt.package(name = "postgresql-14", version = inv["pg_version"])

    if tags.has("production"):
        # full prod tuning
        file.content(path = "/etc/postgresql/14/main/postgresql.conf",
                     contents = template("postgresql.production.conf.j2"),
                     mode = 0o640, owner = "postgres", group = "postgres")
    elif tags.has("staging"):
        # лёгкая стейджовая конфигурация
        file.content(path = "/etc/postgresql/14/main/postgresql.conf",
                     contents = template("postgresql.staging.conf.j2"),
                     mode = 0o640, owner = "postgres", group = "postgres")

    service.systemd(name = "postgresql", enabled = True, state = "started")
```

Преимущества:

- `main.star` краток и не растёт пропорционально количеству ролей × сред.
- Логика среды — рядом с ресурсами, которыми она управляет; контекст не теряется.
- Добавить новую роль = новая директория + одна строка в `main.star`.

### Composition `_lib` → `_lib`

`_lib/foo/main.star` имеет право `load("@lib/bar", ...)`. Цикл `foo` → `bar` → `foo` ловится Starlark'ом на parse-этапе. Поведение симметрично роль → `_lib` и manifest → роль.

## Тестирование

### Уровень 1 — Unit

#### `bosun-core/src/bundle.rs`

- `Bundle::load_dir` на tempdir с минимальным `bundle.toml`.
- `Bundle::resolve_module` для `@roles/X`, `@lib/X`, `@bosun/builtins`, неподдерживаемых паттернов.
- `Bundle::resolve_template` для разных module-context'ов, включая reject из `manifests/main.star`.
- `BundleError` варианты — каждый exercise хотя бы одним тестом.

#### `bosun-core/src/path_safety.rs`

- Each `PathSafetyError` варинт — отдельный тест.
- Symlink-bundle root: canonical resolve однократен.
- Entry с `..`, абсолютный, NUL — все reject.

#### `bosun-core/src/starlark_glue/load_resolver.rs`

- Load `@roles/postgres` из роли с export'ами.
- Load `@lib/runr` из lib.
- `_` private symbol запрещён для import (через filter_private_symbols path).
- Cyclic import (A loads B, B loads A) → LoadCycle error (Starlark уже умеет, мы маппим).
- Module кэш: повторный load — не парсит файл заново. Замеряется `AtomicUsize`-счётчиком, который инкрементируется внутри `parse_and_eval`. Тест: load `@roles/X` дважды → counter == 1.
- `ModuleStackGuard` при panic в eval — стек консистентен (regression test через явный panic в native function).

#### `bosun-core/src/starlark_glue/globals.rs` (inventory + tags)

- `inventory.load(path)` — happy path, missing file, bad yaml, relative-path checks, кеш (вторая загрузка не читает файл — замеряется через mock filesystem или `AtomicUsize`-счётчик).
- `inventory.merge(s1, s2, strategy=...)` — все стратегии, edge cases (null deletion, type mismatch).
- `inventory.merge(s1, s2)` без strategy → берёт из `bundle.toml.[bundle.inventory].default_merge_strategy`.
- `inventory.merge(s1, s2)` без strategy и без default в bundle.toml → fail с сообщением.
- `inventory.merge_keyed(s1, s2, key="id")` — happy path, missing key, non-map element.
- `tags.has` / `require_one_of` / `active` — все варианты.

#### `bosun-primitives/src/template/render.rs` (module-relative resolve)

- Render из role context → `roles/<role>/templates/X.j2`.
- Render из lib context → `_lib/<lib>/templates/X.j2`.
- Render из manifests/main.star → reject TemplateFromManifests.
- Cross-module access (`@roles/...:foo.j2`) → reject.

### Уровень 2 — Golden

Для template — оставляем существующие 3 golden теста (basic_render, with_facts, missing_inv_key), и добавляем по одному per module-context (role, lib).

### Уровень 3 — BDD в Docker

Новая feature: `bundle_structure.feature`:

```gherkin
@docker @bundle
Feature: Bundle directory structure
  bosun applies bundles with role/lib/inventory directory layout.

  @bundle-roles @slow
  Scenario: Multi-role bundle with explicit inventory loading
    Given a fresh container
    And a bundle directory at /work/bundle with:
      | path                                          | content                       |
      | bundle.toml                                   | <see fixture>                 |
      | manifests/main.star                           | <see fixture>                 |
      | inventory/base.yaml                           | service_user: nginx           |
      | inventory/production.yaml                     | service_workers: 8            |
      | roles/nginx/main.star                         | <see fixture>                 |
      | roles/nginx/templates/nginx.conf.j2           | <see fixture>                 |
    When I apply the bundle with tags=production
    Then exit code is 0
    And package "nginx" is installed
    And file "/etc/nginx/nginx.conf" exists in container
    And file "/etc/nginx/nginx.conf" content contains "worker_processes 8;"

  @bundle-tags
  Scenario: Missing --tags fails fast
    Given a fresh container
    And a bundle with main.star calling tags.require_one_of("production", "staging")
    When I apply the bundle without --tags
    Then exit code is 3
    And stderr contains "tags: expected one of"

  @bundle-tags
  Scenario: Wrong tag fails
    Given a fresh container
    And a bundle with main.star calling tags.require_one_of("production", "staging")
    When I apply the bundle with --tags=development
    Then exit code is 3
    And stderr contains "tags: expected one of"

  @bundle-templates
  Scenario: Cross-module template access is rejected
    Given a bundle where role A's main.star calls template("@roles/B:foo.j2")
    When I apply the bundle
    Then exit code is 3
    And stderr contains "cross-module access forbidden"

  @bundle-templates
  Scenario: template() from manifests/main.star is rejected
    Given a bundle where manifests/main.star calls template("foo.j2") at top-level
    When I apply the bundle
    Then exit code is 3
    And stderr contains "cannot be called from manifests/main.star"

  @bundle-privacy
  Scenario: Private symbol import is rejected
    Given a bundle where main.star calls load("@roles/A", "_private")
    When I apply the bundle
    Then exit code is 3
    And stderr contains "private symbol"

  @bundle-composition
  Scenario: Role → lib → template composition resolves correctly
    Given a bundle where:
      | path                                       | content                                          |
      | roles/myrole/main.star                     | imports @lib/runr, calls runr.render_service(name="x") |
      | _lib/runr/main.star                        | defines render_service() which calls template("service.j2") |
      | _lib/runr/templates/service.j2             | [Service]\nExecStart={{ inv.exec }}\n            |
    When I apply
    Then exit code is 0
    And file written contains lib template content (not from bundle-root templates)
```

Дополнительно для `@bundle-inventory`:

```gherkin
  @bundle-inventory
  Scenario: Inventory merge strategy replace replaces all lists
    Given inventory base.yaml { servers: [a, b, c] }
    And inventory prod.yaml { servers: [d] }
    And main.star merges with strategy="replace"
    When I apply with tags=production
    Then the role sees servers = [d]

  @bundle-inventory
  Scenario: Inventory merge strategy deep_map_append_list concats
    Given inventory base.yaml { servers: [a, b, c] }
    And inventory prod.yaml { servers: [d] }
    And main.star merges with strategy="deep_map_append_list"
    Then the role sees servers = [a, b, c, d]

  @bundle-inventory
  Scenario: Null in override removes key
    Given inventory base.yaml { foo: bar, baz: qux }
    And inventory prod.yaml { foo: null }
    When merge
    Then result has only { baz: qux }

  @bundle-inventory
  Scenario: inventory.merge without strategy uses bundle default
    Given bundle.toml [bundle.inventory] default_merge_strategy = "deep_map_append_list"
    And main.star calls inventory.merge(a, b) without strategy=
    Then merge uses deep_map_append_list

  @bundle-inventory
  Scenario: inventory.merge without strategy without bundle default fails
    Given bundle.toml has no [bundle.inventory] section
    And main.star calls inventory.merge(a, b) without strategy=
    Then exit code is 3
    And stderr contains "missing default merge strategy"

  @bundle-validate
  Scenario: bundle validate exits 0 on clean bundle
    Given a fresh bundle that evaluates successfully
    When I run bosun bundle validate --bundle <path> --tags=production
    Then exit code is 0

  @bundle-validate
  Scenario: bundle validate exits 3 on missing inventory file
    Given a bundle whose main.star loads inventory/missing.yaml
    When I run bosun bundle validate --bundle <path> --tags=production
    Then exit code is 3
    And stderr contains "read 'inventory/missing.yaml'"
```

## Migration

Существующий `examples/nginx-demo` переписывается под новый формат (см. секцию «Пример»). Старый формат больше не поддерживается — намеренно. Bundle format breaking change → bumps `bosun-core` version major.

Конкретные сайты потребления, которые нужно мигрировать:

- `bosun-cli/src/run.rs:136` — `bundle.merge_inventory(v)` → заменяется на `inventory.merge(...)` внутри Starlark.
- `bosun-cli/src/run.rs:149` — `bundle.templates_root.clone()` → удалить, templates теперь module-relative.
- `bosun-cli/src/starlark_glue/mod.rs:190` — `bundle.entry_manifest()` → заменяется на `bundle.entry` PathBuf field.
- `bosun-cli/src/starlark_glue/mod.rs:418-419` — `bundle.defaults`, `bundle.manifests.contains_key(...)` → удалить.
- `tests/bdd/bundle_helper.rs:70` — `bundle.merge_inventory` (indirect) → переписать на Starlark inventory в main.star.
- `tests/bdd/bundle_helper.rs:14-16` — legacy workaround для старого `--inventory` флага → удалить.
- Test `Bundle::load_dir::load_manifests_finds_nested_star_files` (bundle.rs:583-602) → удалить; bundle больше не preload'ит manifests.
- 12 тестов `merge_inventory_*` (bundle.rs:476-540) → перенести в `inventory::merge` testbed в `starlark_glue` тестах.
- BDD-сценарии, использующие флаг `--inventory`, переписать через `inventory.load` в main.star: затрагивает `tests/bdd/features/*.feature` (точный список — на стадии имплементации, выставлен grep'ом по `--inventory`).

## Error handling

Расширения PrimitiveError и StarlarkGlueError + новые варианты BundleError (см. выше). Все public enum `#[non_exhaustive]`.

Exit-коды (из MVP):
- 0 — apply ok / dry-run no drift
- 1 — apply partial fail
- 2 — dry-run drift detected
- 3 — manifest/eval error (новые сценарии: ModuleNotFound, UnsupportedLoadPath, PrivateSymbol, cross-module template, tags require fail, TemplateFromManifests, DefaultMergeStrategyMissing, PathSafety попадают сюда)
- 4 — CLI/окружение

## Метрики

Добавляется к существующему набору bosun-метрик (см. MVP-спек):

- `bosun_active_tags{tag="<name>"} 1` — gauge per активный тэг. Пишется в Prometheus textfile на каждом запуске; перезаписывается полностью.

## Out of scope

Намеренно НЕ делаем в этой итерации:

- Bundle tar.gz формат, sha256, MANIFEST, signing/cosign.
- HTTPS distribution через bosun-server.
- Bundle cache layout (/var/lib/bosun/bundles/...).
- Reproducible build (`tar --sort` + `--mtime=@0` + `gzip -n`).
- OCI distribution через oras.
- Secrets store / Vault integration / secret-resolver pluggable interface.
- inventory loading из k8s configmap.
- inline runr.service декларации (вынесено в отдельный runr-integration spec).
- file.content(state="absent") для удаления unit-файлов.
- service.validate (e.g. `nginx -t` перед reload).
- service.health_check после apply.
- Bundle `[[bundle.roles]]` с отдельным role-versioning.
- Bundle.lock с pinning.
- `bundle.conflicts_with` для конфликтующих bundle'ов.
- Multi-bundle на одной ноде (claims на пути).
- Monolithic bundle redeploy size. Operationally known concern: один большой bundle (десятки ролей, сотни templates) переразворачивается целиком при апдейте. Mitigations (split bundles by host-class, content-addressed download) — future Distribution spec.
- Migration tooling — конвертер MVP → новый формат остаётся в future work; ручная конвертация nginx-demo делается в этой итерации.

Все они зафиксированы как future work. Когда дойдём — отдельные design'ы.

## Open questions

- **Максимально глубокий `inventory.merge`-chain.** На 25+ файлах postgres-chiit это 25 cascaded merge'ов. Производительность OK при O(N×size). Optional optimization — `inventory.merge_all([s1, s2, ..., sN], strategy)` один проход. Пока не делаем.

## Связь с существующими концептами

| Концепт wiki | Как используется |
|---|---|
| `concepts/bundle-architecture.md` | бывший «bundle = ядро + bundle на Starlark» — этот спек определяет формат bundle |
| `concepts/runr-supervisor.md` | _lib/runr/ это место где живёт render_service helper для runr.service Phase 11+ |
| `concepts/starlark-dsl.md` | стиль манифестов через side-effect-вызовы остаётся; добавляется multi-file через load + privacy |
| `concepts/chiit-go-dsl.md` | postgres-chiit модель ролей переносится; numbered prefix inventory сохраняется (но загружается явно через `inventory.load`, не auto-scan) |

## Следующие шаги

После имплементации этого spec:

1. Runr integration spec (paused) — возобновить с использованием `_lib/runr/`, обновить с учётом role-based layout.
2. Distribution spec — tar.gz + sha256 + signing + bosun-server endpoint.
3. Secrets primitive + Vault integration.
4. inventory из k8s configmap.
5. Bundle migration tooling — конвертер старого формата (MVP) → нового.

## Замечание о псевдокоде

Все Rust-снипеты в спеке иллюстративны: реализатор обязан пробрасывать ошибки через `?` (а не `unwrap`/`expect`), использовать актуальные имена API стандартной библиотеки и крейтов, проверять граничные случаи (None, empty, NotFound). Сигнатуры и структурные решения — обязательны; конкретные тела функций — справочные.

## История ревизий

### rev 2 (2026-05-19)

Применены поправки двух параллельных ревью (DevOps + Rust architecture).

**Критические:**

- `bundle.toml.entry` валидируется через общий path-safety helper в `Bundle::load_dir` до дальнейшей обработки.
- Symlink rejection и path-safety check явно делегированы единому `path_safety::resolve_within_root` для всех четырёх резолверов (entry, inventory.load, resolve_module, resolve_template).
- Observability для тэгов: Prometheus `bosun_active_tags{tag=}` gauge и `tracing::info!` строка при старте.
- Добавлена CLI команда `bosun bundle validate --bundle <dir> --tags=<csv> [--facts=fixtures/facts.json]` для статической валидации без обращения к системе; описана схема synthetic facts JSON.
- `requires_bosun` зафиксирована точная semver-семантика (Cargo caret rules) + operations guidance (`>=0.4` vs `^0.4`).
- Добавлена секция «Trust boundary в directory mode» — operator контракт о read-only mount, future signed tar.gz замена.
- Privacy enforcement переехал из FileLoader в post-evaluate фильтр модулей (`filter_private_symbols`); 0.13 mechanism уточняется реализатором.
- Module cache key — canonical PathBuf, scoped per-`evaluate_manifest`.
- RAII `ModuleStackGuard` для стека current_module — push/pop через Drop, panic-safe.
- `template()` переработан под call-stack walk (function-defining-module approach): резолвится по defining file текущего user-frame'а, с зафиксированным fallback на per-Module bound closure если 0.13 API не подходит. Reject `template()` из `manifests/main.star`.
- Новый `EvaluatorConfig` объединяет 8+ полей вместо позиционных аргументов в `evaluate_manifest`.
- `Bundle` в `EvalState` — `Rc<Bundle>` (не `Arc`), консистентно с остальными shared полями.

**Важные:**

- `[bundle.inventory].default_merge_strategy` в `bundle.toml`; если не задан и strategy= не указан — fail с понятным сообщением.
- `inventory.load` кеш per-`evaluate_manifest`, keyed by canonical PathBuf, симметрично FileLoader-кешу.
- Добавлены идиомы: «tag-as-filter-inside-role» и `_lib` → `_lib` composition.
- BDD-сценарий cross-module composition (role → lib → template) добавлен.
- Зафиксирована схема `fixtures/facts.json` для `bundle validate`.
- Перечислены конкретные миграционные потребители (run.rs:136/149, mod.rs:190/418-419, bundle_helper.rs:70/14-16, test names и т.п.).
- `inventory.merge` / `merge_keyed` signature в стиле starlark-rust 0.13 (`UnpackTuple`).
- Path-safety helper вынесен в `bosun-core/src/path_safety.rs` с явным `PathSafetyError` enum.
- Cache convention — оба кеша (inventory.load и FileLoader) per-evaluate, canonical PathBuf.
- Error chain: BundleError → StarlarkGlueError → anyhow на FileLoader-границе, downcast обратно в glue-слое.

**Минорные:**

- Отмечено что `serde_norway` сохраняет insertion order — используется для детерминизма merge.
- `deep_map_keyed_list:<key>` заменён на отдельный `inventory.merge_keyed(...)`.
- Роль без `templates/` директории — silent OK пока никто не зовёт `template()`; FileNotFound с подсказкой если зовут.
- Out-of-scope расширен «monolithic bundle redeploy size» как известный operational concern.
- Пояснение `_lib` (filesystem) vs `@lib/` (load namespace) — намеренное расхождение.
- Зафиксировано CLI dedup+sort `--tags` перед передачей в evaluator.
- Добавлено замечание о псевдокоде (использовать `?`, не `unwrap`).
- Зафиксировано: `requires_bosun` behavior относительно MVP не меняется.
