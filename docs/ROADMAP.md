# Roadmap checklist

The working list for building bmv-trader. Each task is done when: tests pass (`scripts/check.sh`), the review for its tier (DESIGN §16) has no unresolved BLOCK, and it's committed. ⛔ marks a stop: it needs the owner (keys, market days, or a decision).

## P0 — Foundation
- [x] Scaffold, paper/live config, `account`, review fixes
- [x] Alpaca paper keys (Keychain or `.env`); `cargo run -- account` prints a validated account
- [ ] ⛔ OmniRoute and Anthropic test calls (needed by P4, not P1)

## P1 — Research lab
- [x] `money.rs`: cents types, strict parsing, exact sizing (tier 1)
- [x] `strategy.rs`: Bar, Candidate, ORB-15 (tier 2)
- [x] `vetting.rs` types: `Verdict { Approve, Reduce(Factor), Reject }` (tier 1, types only; parsing comes in P4)
- [x] `risk.rs`: `Limits::for_mode`, `gate()` for every §9 rule, sizing, open-risk budget, clusters, `self_test()`; property tests (tier 1)
- [x] `engine.rs`: `State`, `Event`, `Effect`, pure `step()`; reservations; one trade per symbol; flatten timing (tier 1)
- [x] `backtest.rs`: replay feeder + simulated broker (§13 fill model), trades → metrics (tier 2)
- [x] Research stats: day-block bootstrap, random-entry baseline, walk-forward split, trial log (tier 2)
- [x] `history.rs`: Alpaca bars `feed=iex`, throttled, resumable cache in `data/`; tested on saved responses (tier 2)
- [x] CLI: `fetch`, `backtest` (tier 3)
- [x] Download history: IEX 2020-08-03 → 2025-12-31, 1,361 sessions, 1 gap day (2025-03-10)
- [ ] ⛔ P1 gate: **not passed** by 7 trials: ORB (1), ORB + trend (2), ORB + marketable (3), gap-and-go on mega-caps (4), VWAP pullback (5), gap-and-go on scanned gappers (6, −0.26R), monthly 12-1 momentum (7, CAGR 9.9% vs SPY 14.9%). Intraday long-only rules and plain momentum closed; owner decides the direction (DECISION R8)

## P2 — Safety core
- [x] Broker orders: bracket submit (idempotent client_order_id), cancel, `close_all`, OCO for partial fills, typed Order/Position (tier 1). Send-once `Bracket`, `Exits` token for the OCO, typed `Cancel`, `close_position`; Alpaca error codes still **[verify codes]**
- [x] Kill switch at `$BMV_HOME/data/STOP` and startup self-tests (`safety.rs`, tier 1)
- [x] Reconcile from Alpaca into the engine; `startup_checks` at boot; kill switch polled every loop (`live.rs`)
- [x] `cargo-mutants` on `risk.rs`: no surviving mutants in limit checks (4 equivalent mutants in the post-sizing `within` check, explained in the QA report)
- [ ] ⛔ Paper tests that resolve every [verify] in DESIGN

## P3 — Paper engine
- [x] Paper run loop v1: `market_data.rs` (REST polling, SPY staleness), `live.rs` (executor, reconcile, JSONL journal, startup: skew, calendar, flat, day P&L), CLI `run` (paper only), `flatten`, `status`
- [x] Monitors: `watch` (terminal) and `ui` (web console on 127.0.0.1: token, Host/Origin checks, CSP; STOP and flatten only; DECISION S16)
- [x] Single-instance lock (`data/run.lock`); flatten backstop `bmv-trader backstop` + launchd job (`deploy/`, owner installs)
- [ ] Websocket market data (replaces REST polling); `report`; X-RateLimit back-off
- [ ] ⛔ 10 unattended paper days; live candidates identical to replay

## P4 — AI layer
- [ ] `llm.rs`, `vetting.rs` parsing, prompt versioning, regression set, drift alarm; second paper account shadow; candidates and verdicts in the `ui` console (tier 1/2)
- [ ] ⛔ 20+ paper days; pre-registered Claude-vs-shadow gate

## Desk: swing book and three layers (DESIGN §4b, §7d; plan approved 2026-11-03)
- [x] Phase 0: design records (DESIGN §1, §4b roles, §7d, §9, §10; DECISION S19–S21, A10–A11, R2i)
- [ ] Phase 1: trial 8, Desk Signal swing: **failed** (−0.52R; cash-like ETFs with tiny stops dominated, DECISION R2i). No swing order until a swing trial passes
- [ ] Phase 2: engine swing book (`bmv-s-` ids, scoped flatten and backstop, GTC stops, risk split, per-book console); tier-1 review
- [ ] Phase 3: data layer, OpenBB sidecar from pinned PyPI (the HEAD checkout crashes on start) plus `src/openbb.rs`
- [ ] Phase 4: `src/llm.rs` tiers, cost ledger and `llm-health`, then the TradingAgents sidecar (Claude-only, social off)
- [ ] Phase 5: agents layer, paperclip (loopback, telemetry off, no broker keys)
- [ ] Phase 6: pre-registered forward paper test, rules-only vs rules + AI vs random vs SPY

## P5 — Live pilot
- [ ] ⛔ Mac mini, phone approvals, live limits, 30 live days
