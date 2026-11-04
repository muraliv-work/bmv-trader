#!/usr/bin/env bash
# Builds desk-sign (the owner's Touch ID signer) into a directory, default target/release: the
# `desk` command runs the desk-sign next to itself, so build both into the same place.
set -euo pipefail
cd "$(dirname "$0")/.."
out="${1:-target/release}"
mkdir -p "$out"
swiftc -O crates/desk/swift/desk-sign.swift -o "$out/desk-sign"
echo "built $out/desk-sign"
