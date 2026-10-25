#!/usr/bin/env bash
# Every feature combination CI builds, in one command.
#
# Why this exists: most crates here have empty default features on purpose (the pure-logic build
# stays free of rusqlite, reqwest and axum), so `cargo test --workspace` compiles only a slice of
# the tree. A change to a shared type — an enum gaining a variant, a trait gaining a method — can
# be clean locally and fail CI on a match that only exists behind `--features db` or `exec`.
# That happened on issue #81 step 3 and cost a red CI round trip; running this before pushing
# turns that into a local failure instead.
#
# ShogunAgent has no macOS shell: every workspace member builds and tests on Linux.
#
# Keep in step with .github/workflows/ci.yml — the combinations below mirror its jobs.
set -euo pipefail

run() {
    echo "── $* "
    "$@"
}

run cargo clippy --workspace --all-targets
run cargo clippy -p shogun-core --features db --all-targets
run cargo clippy -p shogun-core --features exec --all-targets
run cargo clippy -p shogun-core --features daemon-server --all-targets
run cargo clippy -p shogun-mcp --features server --all-targets

run cargo test --workspace
# One run under the superset (daemon-server = exec + server, exec = db + net) executes every
# feature-gated shogun-core test: none is gated on a feature's absence. See ci.yml.
run cargo test -p shogun-core --features daemon-server --locked
run cargo test -p shogun-mcp --features server

echo "feature matrix: OK"
