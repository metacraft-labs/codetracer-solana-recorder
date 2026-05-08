set shell := ["bash", "-euo", "pipefail", "-c"]

# Default recipe — list available recipes.
default:
  @just --list

build:
  cargo build --locked

build-release:
  cargo build --release --locked

test:
  cargo test --locked
  bash tests/verify-cli-convention-no-silent-skip.sh

t: test

test-rust:
  cargo test --locked

verify-cli-convention:
  bash tests/verify-cli-convention-no-silent-skip.sh

lint:
  cargo fmt --check
  cargo clippy --locked --all-targets -- -D warnings
  bash tests/verify-cli-convention-no-silent-skip.sh

format:
  cargo fmt

fmt: format
