VERSION 0.8

# Reproducible Linux builds for BugStalker.
# Default target platform is linux/arm64 because the aarch64 port is the
# current work; switch with:  earthly --BS_PLATFORM=linux/amd64 +check
ARG --global BS_PLATFORM=linux/arm64

common:
    FROM --platform=$BS_PLATFORM rust:1.89-bookworm
    ENV CARGO_TERM_COLOR=always
    ENV DEBIAN_FRONTEND=noninteractive
    RUN apt-get update && \
        apt-get install -y --no-install-recommends \
            build-essential \
            pkg-config \
            libc6-dbg \
            python3 \
            python3-pip \
            ca-certificates \
            git && \
        rm -rf /var/lib/apt/lists/*
    WORKDIR /bs

source:
    FROM +common
    COPY Cargo.toml Cargo.lock build.rs rust-toolchain.toml ./
    COPY --dir src tests ./

examples-source:
    FROM +source
    COPY --dir examples ./

# cargo check — fastest feedback loop for the port.
check:
    FROM +source
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target \
        cargo check --all-targets

# cargo build (debug) — exports the `bs` binary to target/earthly/<arch>/.
build:
    FROM +source
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        cargo build
    SAVE ARTIFACT target/debug/bs AS LOCAL target/earthly/$BS_PLATFORM/bs

build-rel:
    FROM +source
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        cargo build --release
    SAVE ARTIFACT target/release/bs AS LOCAL target/earthly/$BS_PLATFORM/bs-rel

clippy:
    FROM +source
    RUN rustup component add clippy
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target \
        cargo clippy --all-targets -- -D warnings

fmt-check:
    FROM +source
    RUN rustup component add rustfmt
    RUN cargo fmt --all -- --check

# Build the tree of example debuggee binaries at examples/target/debug/*.
# The functional test suite in tests/debugger/ spawns these as the debuggee.
build-examples:
    FROM +examples-source
    WORKDIR /bs/examples
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/examples/target,sharing=locked \
        cargo build -p calc_lib && \
        cargo build && \
        # Copy the built artifacts out of the cache mount so subsequent
        # image layers can see them.
        mkdir -p /bs/examples/_built && \
        cp -r target/debug /bs/examples/_built/debug
    WORKDIR /bs

# Library + functional tests. ptrace needs SYS_PTRACE, so run privileged.
# We can't also mount /bs/examples/target as a cache here because we
# need the example binaries visible inside the image.
test:
    FROM +build-examples
    RUN mkdir -p examples/target && \
        mv examples/_built/debug examples/target/debug
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        cargo test --features int_test

# Quicker feedback loop: lib unit tests only.
cargo-test-lib:
    FROM +source
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        cargo test --lib

# Debugger integration tests only (tests/debugger/). Useful for aarch64
# triage so the DAP tests don't halt cargo before these run.
test-debugger:
    FROM +build-examples
    RUN mkdir -p examples/target && \
        mv examples/_built/debug examples/target/debug
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        cargo test --features int_test --test debugger -- --test-threads=1

# Run a single test with debug logs — triage tool for the aarch64 port.
# Usage: earthly -P +trace --TEST=test_debugger_runs
trace:
    ARG TEST=test_debugger_runs
    FROM +build-examples
    RUN mkdir -p examples/target && \
        mv examples/_built/debug examples/target/debug
    ENV RUST_LOG=debug
    ENV RUST_BACKTRACE=1
    # No target cache here — triage builds must compile fresh so any
    # source edits land in the test binary.
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        cargo test --features int_test --test debugger -- --test-threads=1 --nocapture $TEST

all:
    BUILD +check
    BUILD +clippy
    BUILD +fmt-check
