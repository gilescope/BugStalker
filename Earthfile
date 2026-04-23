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

# Unit/functional tests. ptrace needs SYS_PTRACE, so run privileged.
test:
    FROM +examples-source
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target \
        cargo test --features int_test

all:
    BUILD +check
    BUILD +clippy
    BUILD +fmt-check
