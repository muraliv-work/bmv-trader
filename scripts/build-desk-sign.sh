#!/usr/bin/env bash
# Builds desk-sign (the owner's Touch ID signer) into target/desk-sign.
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p target
swiftc -O crates/desk/swift/desk-sign.swift -o target/desk-sign
echo "built target/desk-sign"
