.PHONY: build test test-unit test-bdd docker-base bosun-bookworm fmt clippy

BUILD_PROFILE ?= release
BOSUN_TEST_IMAGE ?= bosun-test-base:latest
BOOKWORM_RUST_IMAGE ?= rust:1-bookworm

build:
	cargo build --workspace --$(BUILD_PROFILE)

test: test-unit

test-unit:
	cargo test --workspace --lib --bins --tests

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings -D clippy::unwrap_used -D clippy::expect_used

docker-base:
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

# Run BDD: make test-bdd
# Filter by tag: make test-bdd TAGS=@apt-package
# Combine tags: make test-bdd TAGS='@apt-package and not @slow'
test-bdd: docker-base bosun-bookworm
	BOSUN_TEST_IMAGE=$(BOSUN_TEST_IMAGE) \
	BOSUN_BINARY=$(CURDIR)/target/bookworm-release/bosun \
	cargo test -p bosun-cli --features bdd --test bdd --release -- $(if $(TAGS),--tags '$(TAGS)',)
