build:
	cargo build

build-rel:
	cargo build --release

build-test:
	cargo build --features "int_test"

build-test-rel:
	cargo build --release --features "int_test"

RUST_VERSION ?= stable

build-examples-for-func-test:
	cd examples; \
	cargo +$(RUST_VERSION) build -p calc_lib; \
	$(SHLIB_SO_PATH) cargo +$(RUST_VERSION) build; \

build-examples: build-examples-for-func-test
	cd examples; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_tcp/tokio_1_40/Cargo.toml; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_tcp/tokio_1_41/Cargo.toml; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_tcp/tokio_1_42/Cargo.toml; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_tcp/tokio_1_43/Cargo.toml; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_tcp/tokio_1_44/Cargo.toml; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_vars/tokio_1_40/Cargo.toml; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_vars/tokio_1_41/Cargo.toml; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_vars/tokio_1_42/Cargo.toml; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_vars/tokio_1_43/Cargo.toml; \
	cargo +$(RUST_VERSION) build --manifest-path tokio_vars/tokio_1_44/Cargo.toml; \

build-all: build build-examples

build-all-rel: build-rel build-examples

# Phase 0 tooling: developer entry points are nextest by default
# (per project memory: 20× faster on Darwin than `cargo test`).
# Plain `cargo test` remains available for CI shapes that don't yet
# have nextest installed.
NEXTEST ?= cargo nextest run

cargo-test:
	cargo test --features "int_test"

# Preferred local entry point. `nt` = nextest.
nt:
	$(NEXTEST) --workspace

nt-int:
	$(NEXTEST) --workspace --features "int_test"

# Legacy Python integration tests were retired (see tests/integ/).
# These targets are aliases to the Rust runner for muscle memory.
int-test: build-test
	cargo test --test integ --features int_test -- --test-threads=1

int-test-rel: build-test-rel
	cargo test --test integ --features int_test -- --test-threads=1

test: build-all cargo-test int-test

test-rel: build-all-rel cargo-test int-test-rel

# Phase 0 single-shot lint: clippy + fmt-check across the workspace.
lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

# Phase 0 bench entry point. Runs in `--quick` mode for CI; drop the
# `BENCH_FLAGS` override to get the full Criterion sample set locally.
BENCH_FLAGS ?= -- --quick
bench:
	cargo bench --workspace $(BENCH_FLAGS)

# `cargo deny check` runs the licence allow-list, advisories, and
# source allow-list. Configured in `deny.toml`.
deny:
	cargo deny check

# Phase 0 fuzz placeholder. Real `cargo fuzz` targets land in Phase 8;
# until then this just builds the workspace under sanitizer-friendly
# flags so the fuzz infrastructure exists.
fuzz:
	@echo "fuzz: no targets registered yet — see doc/plans/phase-8-testing.md"

clean-all:
	find . -name Cargo.toml -print0 | xargs -0 -n1 dirname | xargs -n1 -I{} sh -c 'echo ">> cleaning {}"; (cd "{}" && cargo clean)'

install:
	cargo install --path .

.PHONY: build build-rel build-test build-test-rel build-examples-for-func-test build-examples build-all build-all-rel cargo-test nt nt-int int-test int-test-rel test test-rel lint bench deny fuzz clean-all install
