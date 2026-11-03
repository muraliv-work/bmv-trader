#!/usr/bin/env bash
# The gate for every commit: format, lint, test. Usage: scripts/check.sh
set -euo pipefail
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cd "$(dirname "$0")/.."
cargo fmt --check
cargo clippy -q --all-targets -- -D warnings
cargo test -q
echo "check: OK"
