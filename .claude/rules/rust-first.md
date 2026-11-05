# Rust first (owner's rule, 2026-11-05)

New code in this project is written in **Rust**, not Python.

- **New logic** (parsing, money, keys, API calls, file writes, setup steps) goes in a Rust crate,
  as a library function with tests and, when a script or launchd job needs it, a `bmv-trader` or
  `desk-*` subcommand.
- **Shell scripts stay thin:** they call system tools (`security`, `dscl`, `launchctl`, `sudo`) and
  Rust subcommands. They don't grow logic of their own, and new ones don't embed `python3 -c`.
- **When a script needs JSON or a computation,** add or extend a Rust subcommand for it, the way
  `desk-agent --list` serves the setup scripts.
- **Existing Python** is replaced when that code is next changed, not in a big rewrite:
  `scripts/clip-agents-sync.py`, `scripts/tree-hash.py` (and its test), and the `python3 -I -c`
  snippets inside `scripts/*.sh`.
- **Exceptions, which stay Python:**
  - third-party Python sidecars run as-is, such as `sidecars/tradingagents` and `sidecars/openbb`.
    Their own code isn't ours to rewrite, so only the thin glue around them is
  - one-off checks in the scratchpad that are never committed
