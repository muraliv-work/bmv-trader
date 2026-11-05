#!/usr/bin/env bash
# The gate for every commit: format, lint, test. Usage: scripts/check.sh
set -euo pipefail
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy -q --workspace --all-targets -- -D warnings
cargo test -q --workspace
for s in scripts/*.sh; do bash -n "$s"; done
python3 -I scripts/tree-hash-test.py
echo "check: OK"
