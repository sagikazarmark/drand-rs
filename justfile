# `native-tls` adds no API and needs system OpenSSL (openssl-sys) on Linux,
# which the dev shell lacks, so feature matrices skip it.
hack := "--workspace --exclude-features native-tls"

wasm := "wasm32-unknown-unknown"

# The client without reqwest (bring-your-own transport, `TowerTransport`): not
# an `--each-feature` combination, so `clippy`, `test` and `doc` run it explicitly.
no_reqwest := "-p drand --no-default-features --features client,tower,test-chain"

# List the recipes.
[private]
default:
    @just --list --unsorted

# Run all CI jobs.
ci: fmt check clippy test doc build audit deny

# Build the workspace.
build:
    cargo build --workspace --all-targets --locked
    cargo build --workspace --target {{ wasm }} --locked

# Check formatting.
fmt:
    cargo fmt --all --check

# Check the workspace.
check:
    cargo hack check {{ hack }} --feature-powerset --all-targets --locked
    cargo hack check {{ hack }} --feature-powerset --target {{ wasm }} --locked

# Run clippy.
clippy:
    cargo hack clippy {{ hack }} --each-feature --all-targets --locked -- --deny warnings
    cargo hack clippy {{ hack }} --each-feature --target {{ wasm }} --locked -- --deny warnings
    cargo clippy {{ no_reqwest }} --all-targets --locked -- --deny warnings

# Run tests.
test:
    cargo hack test {{ hack }} --each-feature --locked
    cargo test {{ no_reqwest }} --locked

# Run rustdoc.
[env("RUSTDOCFLAGS", "--deny warnings")]
doc:
    cargo hack doc {{ hack }} --each-feature --no-deps --locked
    cargo doc {{ no_reqwest }} --no-deps --locked

# Audit dependencies for security advisories.
audit:
    cargo audit

# Check dependency licenses, bans and sources (advisories are `audit`'s job).
deny:
    cargo deny --locked check bans licenses sources
