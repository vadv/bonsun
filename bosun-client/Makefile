.PHONY: build test test-unit test-bdd test-bdd-systemd docker-base bosun-bookworm \
        runr-bookworm fmt clippy musl-x86_64 musl-aarch64 musl-docker musl-verify

BUILD_PROFILE ?= release
BOSUN_TEST_IMAGE ?= bosun-test-base:latest
BOOKWORM_RUST_IMAGE ?= rust:1-bookworm
# Источник runr: исходники локального проекта. Для BDD сценариев `@runr-service`
# собираем настоящий runr-бинарь поверх той же bookworm-glibc, что и bosun
# (см. цель runr-bookworm). По умолчанию ищется в `../runr` (sibling), затем
# в `../../runr` (родительская группа проектов вида `~/Projects/<group>/{bosun-client,runr}`).
# CI и нестандартные layout'ы переопределяют переменную явно.
RUNR_SOURCE_DIR ?= $(shell \
    if [ -f $(CURDIR)/../runr/Cargo.toml ]; then echo $(CURDIR)/../runr; \
    elif [ -f $(CURDIR)/../../runr/Cargo.toml ]; then echo $(CURDIR)/../../runr; \
    fi)
# Образ для воспроизводимой musl-сборки: rust:alpine содержит musl-toolchain
# из коробки, дополнительные пакеты ставит сам Dockerfile.
MUSL_BUILD_IMAGE ?= bosun-musl-build:latest

build:
	cargo build --workspace --$(BUILD_PROFILE)

test: test-unit

test-unit:
	cargo test --workspace --lib --bins --tests

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings -D clippy::unwrap_used -D clippy::expect_used

docker-base: runr-bookworm
	docker build -t $(BOSUN_TEST_IMAGE) -f docker/test-base.Dockerfile .

# Сборка bosun в bookworm-контейнере: получаем бинарь, совместимый по glibc
# с debian:bookworm-slim. Кладём в target/bookworm-release/bosun, чтобы не
# перетирался обычной хост-сборкой.
bosun-bookworm:
	mkdir -p target/bookworm-release
	docker run --rm \
	  -v $(CURDIR):/work \
	  -v $(CURDIR)/target/bookworm-release:/out \
	  -w /work \
	  -e CARGO_TARGET_DIR=/tmp/cargo-target \
	  $(BOOKWORM_RUST_IMAGE) \
	  sh -c "cargo build --release -p bosun-cli && cp /tmp/cargo-target/release/bosun /out/bosun"

# Сборка runr-binary в том же bookworm-контейнере, что и bosun: bdd-образ
# `bosun-test-base` строится поверх `debian:bookworm-slim`, а runr с GLIBC_2.34+
# из host-fedora не загрузится. Артефакт кладётся в target/runr-bookworm/runr
# и копируется в test-base.Dockerfile через `COPY`. Источник — локальный
# проект runr (RUNR_SOURCE_DIR), не upstream; ENV переменная даёт точку
# подмены для CI с другим расположением.
runr-bookworm:
	@test -d "$(RUNR_SOURCE_DIR)" || { echo "RUNR_SOURCE_DIR=$(RUNR_SOURCE_DIR) does not exist; set it to a checkout of the runr repo"; exit 1; }
	mkdir -p target/runr-bookworm
	docker run --rm \
	  -v $(RUNR_SOURCE_DIR):/work \
	  -v $(CURDIR)/target/runr-bookworm:/out \
	  -w /work \
	  -e CARGO_TARGET_DIR=/tmp/cargo-target \
	  $(BOOKWORM_RUST_IMAGE) \
	  sh -c "cargo build --release && cp /tmp/cargo-target/release/runr /out/runr"

# Run BDD: make test-bdd
# Filter by tag: make test-bdd TAGS=@apt-package
# Combine tags: make test-bdd TAGS='@apt-package and not @slow'
test-bdd: runr-bookworm docker-base bosun-bookworm
	BOSUN_TEST_IMAGE=$(BOSUN_TEST_IMAGE) \
	BOSUN_BINARY=$(CURDIR)/target/bookworm-release/bosun \
	cargo test -p bosun-cli --features bdd --test bdd --release -- $(if $(TAGS),--tags '$(TAGS)',)

# Сценарии `@systemd-privileged` запускают настоящий systemd как PID 1
# в `docker run --privileged --cgroupns=private`. Обычный `test-bdd`
# отфильтровывает их, чтобы сборка работала и без privileged-docker
# (например, в rootless-CI). Эта цель ставит флаг и фильтрует обратно
# только эти сценарии — остальные skip'аются.
#
# Требует: docker, имеющий право поднимать privileged-контейнеры на
# host'е (rootless docker не подходит — systemd сам монтирует tmpfs
# в /sys/fs/cgroup внутри namespace'а).
test-bdd-systemd: runr-bookworm docker-base bosun-bookworm
	BOSUN_TEST_IMAGE=$(BOSUN_TEST_IMAGE) \
	BOSUN_BINARY=$(CURDIR)/target/bookworm-release/bosun \
	BDD_SYSTEMD_PRIVILEGED=1 \
	cargo test -p bosun-cli --features bdd --test bdd --release -- --tags '@systemd-privileged'

# Статический musl-бинарь под x86_64. Требует на сборочной машине
# `musl-tools` (Debian/Ubuntu) или `musl-dev` (Alpine). На target-узле
# никаких рантайм-зависимостей не требуется — `ldd` показывает
# "statically linked".
musl-x86_64:
	rustup target add x86_64-unknown-linux-musl
	cargo build --release --target x86_64-unknown-linux-musl --bin bosun
	@echo "binary: $(CURDIR)/target/x86_64-unknown-linux-musl/release/bosun"
	@file $(CURDIR)/target/x86_64-unknown-linux-musl/release/bosun
	@ldd $(CURDIR)/target/x86_64-unknown-linux-musl/release/bosun 2>&1 || true

# Статический musl-бинарь под aarch64. На хосте x86_64 без
# aarch64-linux-musl-gcc собирать не получится — для воспроизводимой
# сборки используется `make musl-docker TARGET=aarch64-unknown-linux-musl`.
musl-aarch64:
	rustup target add aarch64-unknown-linux-musl
	cargo build --release --target aarch64-unknown-linux-musl --bin bosun
	@echo "binary: $(CURDIR)/target/aarch64-unknown-linux-musl/release/bosun"
	@file $(CURDIR)/target/aarch64-unknown-linux-musl/release/bosun

# Воспроизводимая сборка musl-бинаря в контейнере rust:alpine. Не требует
# musl-toolchain на хосте, только docker. TARGET по умолчанию — текущая
# архитектура хоста; для cross-build на ту же арку bypass-ом достаточно.
#
# Готовый бинарь кладётся в target/musl-release/bosun для удобства CI
# (отдельная директория, чтобы не путать с локальной cargo-сборкой).
TARGET ?= x86_64-unknown-linux-musl
musl-docker:
	docker build -t $(MUSL_BUILD_IMAGE) -f docker/musl-build.Dockerfile .
	mkdir -p $(CURDIR)/target/musl-release
	# В rust:alpine системный gcc уже musl-aware (вся дистрибуция собрана
	# поверх musl-libc), поэтому переопределяем линкер через env, иначе
	# .cargo/config.toml ищет musl-gcc, которого в alpine нет.
	docker run --rm \
	  -v $(CURDIR):/work:ro \
	  -v $(CURDIR)/target/musl-release:/out \
	  -e TARGET=$(TARGET) \
	  -e CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc \
	  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=cc \
	  $(MUSL_BUILD_IMAGE) \
	  sh -c "cd /work && cargo build --release --target $(TARGET) --bin bosun --target-dir /tmp/cargo-target && cp /tmp/cargo-target/$(TARGET)/release/bosun /out/bosun"
	@echo "binary: $(CURDIR)/target/musl-release/bosun"
	@file $(CURDIR)/target/musl-release/bosun

# Smoke-test musl-бинаря в distroless/static контейнере — там нет вообще
# никаких shared libraries и нет libc. Если бинарь не статический, он
# просто не запустится.
MUSL_BINARY ?= $(CURDIR)/target/x86_64-unknown-linux-musl/release/bosun
musl-verify:
	@test -x "$(MUSL_BINARY)" || { echo "no binary at $(MUSL_BINARY); run make musl-x86_64 first"; exit 1; }
	@echo "--- file ---"
	@file $(MUSL_BINARY)
	@echo "--- ldd ---"
	@ldd $(MUSL_BINARY) 2>&1 || true
	@echo "--- size ---"
	@ls -lh $(MUSL_BINARY) | awk '{print $$5, $$9}'
	@echo "--- smoke test in distroless/static ---"
	docker run --rm -v $(MUSL_BINARY):/bosun:ro gcr.io/distroless/static-debian12 /bosun --version
