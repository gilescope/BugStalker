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
    COPY Cargo.toml Cargo.lock build.rs rust-toolchain.toml deny.toml ./
    # `crates/` hosts the workspace member crates introduced in Phase 0;
    # `benches/` hosts the criterion bench scaffolding.
    COPY --dir src tests crates benches ./

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
    # `examples/target` may be a (broken) symlink on hosts that redirect
    # cargo target dirs out of the source tree — strip it before recreating.
    RUN rm -rf examples/target && \
        mkdir -p examples/target && \
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
    # `examples/target` may be a (broken) symlink on hosts that redirect
    # cargo target dirs out of the source tree — strip it before recreating.
    RUN rm -rf examples/target && \
        mkdir -p examples/target && \
        mv examples/_built/debug examples/target/debug
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        cargo test --features int_test --test debugger -- --test-threads=1

# Run a single test with debug logs — triage tool for the aarch64 port.
# Usage: earthly -P +trace --TEST=test_debugger_runs
trace:
    ARG TEST=test_debugger_runs
    FROM +build-examples
    # `examples/target` may be a (broken) symlink on hosts that redirect
    # cargo target dirs out of the source tree — strip it before recreating.
    RUN rm -rf examples/target && \
        mkdir -p examples/target && \
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

# Phase 0 single-shot lint target. Composes clippy + fmt-check.
lint:
    BUILD +clippy
    BUILD +fmt-check

# Phase 0 bench placeholder — runs the criterion suites in --quick mode.
# Real benches land per-crate as the workspace grows; see
# `benches/render_value.rs`, `benches/attach_cold.rs`.
bench:
    FROM +source
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target \
        cargo bench --workspace -- --quick

# Phase 1 acceptance smoke. Drives `bugstalker` against the
# `examples/vars` debuggee through the library API and asserts every
# Phase 1 stdlib type renders via its specialised path. Then runs the
# bench suite in --quick mode and fails on `Performance has
# regressed` (criterion compares against the baseline saved under
# `target/criterion/` from the previous run; cargo cache persists
# across Earthly invocations of this target so the comparison is
# meaningful in CI).
#
# First-run behaviour: criterion has no baseline yet, so the
# regression-grep matches nothing and the bench step passes
# unconditionally. Subsequent runs gate on >=5 % stat-significant
# slowdown (criterion's default thresholds).
smoke:
    FROM +build-examples
    # `examples/target` may be a (broken) symlink on hosts that redirect
    # cargo target dirs out of the source tree — strip it before recreating.
    RUN rm -rf examples/target && \
        mkdir -p examples/target && \
        mv examples/_built/debug examples/target/debug
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        cargo run -p bs-smoke -- --vars examples/target/debug/vars
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        bash -c 'set -o pipefail; \
            cargo bench --workspace -- --quick 2>&1 | tee /tmp/bench.log; \
            if grep -q "Performance has regressed" /tmp/bench.log; then \
                echo "[bs/smoke] benchmark regression detected — see /tmp/bench.log"; \
                exit 1; \
            fi'

# `cargo deny check` enforces the licence allow-list and surfaces
# advisories. Configured in `deny.toml`.
deny:
    FROM +common
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        cargo install cargo-deny --locked
    COPY Cargo.toml Cargo.lock deny.toml ./
    COPY --dir src tests crates ./
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        cargo deny check

# Phase 0 fuzz placeholder. `cargo fuzz` targets are introduced in
# Phase 8; this target exists so the CI matrix has a stable surface.
fuzz:
    FROM +source
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        cargo install cargo-fuzz --locked || true
    RUN echo "fuzz: no targets registered yet — see doc/plans/phase-8-testing.md"

# Repeat a single test N times (default 20). Useful for hunting flakes.
flake-hunt:
    ARG TEST=test_debug_trait_repr_vars
    ARG N=20
    FROM +build-examples
    # `examples/target` may be a (broken) symlink on hosts that redirect
    # cargo target dirs out of the source tree — strip it before recreating.
    RUN rm -rf examples/target && \
        mkdir -p examples/target && \
        mv examples/_built/debug examples/target/debug
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        cargo build --tests --features int_test --test debugger
    RUN --privileged \
        for i in $(seq 1 $N); do \
            echo "[flake-hunt] iteration $i/$N"; \
            ./target/debug/deps/debugger-* --test-threads=1 --exact $TEST \
                || { echo "[flake-hunt] FAILED at iteration $i"; exit 1; }; \
        done

# macOS-only convenience: cargo-install `bs`, codesign with the
# debugger entitlements, and alias `bugstalker` -> `bs` so the VS
# Code extension's default executable name resolves on PATH.
#
# *Adhoc* sign without Hardened Runtime — that's the combination
# that lets the `com.apple.security.cs.debugger` entitlement
# actually take effect for `task_for_pid` on a dev machine. With
# Hardened Runtime (`-o runtime`) macOS treats `cs.debugger` as a
# restricted entitlement and silently drops it from an adhoc
# signature; the binary then gets KERN_FAILURE at attach time.
# A real release build would use a Developer ID signature plus
# notarization, which can carry restricted entitlements under
# Hardened Runtime — that path is `+install-darwin-release`,
# not this one.
#
# Runs LOCALLY — codesign needs the host's keychain + Apple toolchain
# and has no container equivalent.
#
# Usage:  earthly +install-darwin
install-darwin:
    LOCALLY
    RUN test "$(uname)" = Darwin || \
        { echo "+install-darwin: macOS only (host is $(uname))"; exit 1; }
    RUN cargo install --path . --bin bs --force
    RUN codesign -s - --force \
            --entitlements tests/darwin.entitlements \
            "$HOME/.cargo/bin/bs"
    # Verify the entitlement actually landed. If this fails the
    # most common cause is a stale signature surviving --force on
    # some macOS versions; re-run after `codesign --remove-signature`.
    RUN codesign -d --entitlements - "$HOME/.cargo/bin/bs" 2>&1 \
        | grep -q 'com.apple.security.cs.debugger' || \
        { echo "+install-darwin: cs.debugger entitlement missing after codesign"; exit 1; }
    RUN ln -sf bs "$HOME/.cargo/bin/bugstalker"
    RUN echo "+install-darwin: bs installed at \$HOME/.cargo/bin/bs (adhoc-signed with cs.debugger), bugstalker symlink in place"

# ----------------------------------------------------------------------
# CI-mirror targets
# ----------------------------------------------------------------------
#
# Each `+ci-*` target reproduces one job from `.github/workflows/ci.yml`
# as closely as a container can. Use these to triage CI failures
# locally without round-tripping through GitHub Actions.
#
# Single-job:   earthly -P +ci-lint
# Whole suite:  earthly -P +ci-all
# arm64 too:    earthly -P --BS_PLATFORM=linux/arm64 +ci-all
# macOS smoke:  earthly +ci-test-macos     (LOCALLY — needs a macOS host)
# Nix check:    earthly +ci-nix            (LOCALLY — needs host `nix`)

# Mirrors CI's `test` job. Parameterised by `RUSTC` so the same target
# covers the 1.91 … 1.95 matrix.
#   - installs RUSTC + MSRV
#   - builds the example debuggees with RUSTC
#   - greps the `rustc version` string out of `calc` as a sanity check
#   - runs `cargo test` against the bs library
ci-test:
    ARG RUSTC=1.95.0
    FROM +common
    RUN rustup toolchain install "$RUSTC" --profile minimal && \
        rustup toolchain install 1.89.0 --profile minimal
    COPY Cargo.toml Cargo.lock build.rs rust-toolchain.toml deny.toml ./
    COPY --dir src tests crates benches examples ./
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/examples/target,sharing=locked \
        cd examples && \
        cargo "+$RUSTC" build -p calc_lib && \
        cargo "+$RUSTC" build && \
        mkdir -p /bs/examples/_built && \
        cp -r target/debug /bs/examples/_built/debug
    RUN strings examples/_built/debug/calc | grep "^rustc version" | grep "$RUSTC"
    RUN rm -rf examples/target && mkdir -p examples/target && \
        mv examples/_built/debug examples/target/debug
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        cargo test

# 5-version matrix — the same shape CI runs.
ci-test-matrix:
    BUILD +ci-test --RUSTC=1.91.0
    BUILD +ci-test --RUSTC=1.92.0
    BUILD +ci-test --RUSTC=1.93.0
    BUILD +ci-test --RUSTC=1.94.0
    BUILD +ci-test --RUSTC=1.95.0

# Mirrors CI's `integration-test` job (the python unittest suite).
# Builds bs in release mode, builds the examples with LRV, then runs
# `make int-test-rel`.
ci-integration-test:
    FROM +common
    # `int-test-rel` invokes `sudo python3 -m unittest`; sudo isn't
    # in `rust:1.89-bookworm` and Earthly's container UID is root
    # anyway, so wire `sudo` to a no-op alias.
    RUN echo '#!/bin/sh' > /usr/local/bin/sudo && \
        echo 'exec "$@"' >> /usr/local/bin/sudo && \
        chmod +x /usr/local/bin/sudo
    RUN rustup toolchain install 1.95.0 --profile minimal && \
        rustup toolchain install 1.89.0 --profile minimal
    COPY Cargo.toml Cargo.lock build.rs rust-toolchain.toml deny.toml Makefile ./
    COPY --dir src tests crates benches examples ./
    COPY requirements.txt ./
    RUN pip3 install --break-system-packages -r requirements.txt
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        make build-rel
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/examples/target,sharing=locked \
        make build-examples RUST_VERSION=1.95.0 && \
        mkdir -p /bs/examples/_built && \
        cp -r examples/target/debug /bs/examples/_built/debug
    RUN rm -rf examples/target && mkdir -p examples/target && \
        mv examples/_built/debug examples/target/debug
    RUN strings ./target/release/bs | grep "rustc version" | grep "1.89.0" && \
        strings ./examples/target/debug/calc | grep "^rustc version" | grep "1.95.0"
    RUN --privileged make int-test-rel

# Mirrors CI's `lint` job: cargo build (workspace + examples), MSRV
# string check, fmt --check, clippy -D warnings, all on MSRV.
ci-lint:
    FROM +common
    RUN rustup toolchain install 1.89.0 --profile minimal \
            --component rustfmt --component clippy && \
        rustup override set 1.89.0
    COPY Cargo.toml Cargo.lock build.rs rust-toolchain.toml deny.toml Makefile ./
    COPY --dir src tests crates benches examples ./
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        --mount=type=cache,target=/bs/examples/target,sharing=locked \
        make build-all
    RUN grep '^rust-version = .1\.89\.0.' Cargo.toml
    RUN cargo fmt --all -- --check
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        cargo clippy -- -D warnings

# Mirrors CI's `deny` job. Runs the full set of cargo-deny checks
# the EmbarkStudios action runs in CI: licenses, bans, sources,
# advisories — all with `--all-features` so transitive deps gated
# by features are still scanned.
ci-deny:
    FROM +common
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        cargo install cargo-deny --locked
    COPY Cargo.toml Cargo.lock deny.toml ./
    COPY --dir src tests crates examples ./
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        cargo deny --all-features --manifest-path ./Cargo.toml \
            check licenses bans sources advisories

# Mirrors CI's `test-arm64` job. Same as `ci-test` plus the
# explicit `--features int_test --test debugger` and `--test dap`
# passes. Always uses LRV (1.95). On a multi-arch host, target the
# arm64 platform explicitly: `--BS_PLATFORM=linux/arm64`.
ci-test-arm64:
    FROM +common
    RUN rustup toolchain install 1.95.0 --profile minimal && \
        rustup toolchain install 1.89.0 --profile minimal
    COPY Cargo.toml Cargo.lock build.rs rust-toolchain.toml deny.toml ./
    COPY --dir src tests crates benches examples ./
    RUN --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/examples/target,sharing=locked \
        cd examples && \
        cargo +1.95.0 build -p calc_lib && \
        cargo +1.95.0 build && \
        mkdir -p /bs/examples/_built && \
        cp -r target/debug /bs/examples/_built/debug
    RUN rm -rf examples/target && mkdir -p examples/target && \
        mv examples/_built/debug examples/target/debug
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        cargo test
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        cargo test --features int_test --test debugger -- --test-threads=1
    RUN --privileged \
        --mount=type=cache,target=/usr/local/cargo/registry \
        --mount=type=cache,target=/bs/target,sharing=locked \
        cargo test --features int_test --test dap

# Same shape as `ci-lint`; arch hint is from --BS_PLATFORM.
ci-lint-arm64:
    BUILD +ci-lint

# Mirrors CI's `nix` job. `nix flake check` needs a host nix install
# and can't easily run inside an Earthly container, so this target
# is LOCALLY.
ci-nix:
    LOCALLY
    RUN command -v nix > /dev/null || { \
        echo "+ci-nix: host \`nix\` not found; install Nix first"; exit 1; }
    RUN nix flake check

# Mirrors CI's `test-macos` smoke job: cargo check --workspace
# --all-targets, then cargo test --workspace --lib. macOS only.
ci-test-macos:
    LOCALLY
    RUN test "$(uname)" = Darwin || \
        { echo "+ci-test-macos: macOS only (host is $(uname))"; exit 1; }
    RUN rustup toolchain install 1.95.0 && rustup default 1.95.0
    RUN cargo check --workspace --all-targets
    RUN cargo test --workspace --lib

# Full CI sweep: every container-capable job in parallel. `nix` and
# `test-macos` are host-bound (LOCALLY) and are not BUILD-able from
# inside another target; run them by hand from a darwin / nix host.
ci-all:
    BUILD +ci-test-matrix
    BUILD +ci-integration-test
    BUILD +ci-lint
    BUILD +ci-deny
    BUILD +ci-test-arm64
