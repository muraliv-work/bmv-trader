# Roadmap checklist

The working list for building bmv-trader. Each task is done when: tests pass (`scripts/check.sh`), the review for its tier (DESIGN §16) has no unresolved BLOCK, and it's committed. ⛔ marks a stop: it needs the owner (keys, market days, or a decision).

## P0 — Foundation
- [x] Scaffold, paper/live config, `account`, review fixes
- [ ] ⛔ Alpaca paper keys in `.env`; `cargo run -- account` prints a validated account
- [ ] ⛔ OmniRoute and Anthropic test calls (needed by P4, not P1)

## P1 — Research lab
- [x] `money.rs`: cents types, strict parsing, exact sizing (tier 1)
- [x] `strategy.rs`: Bar, Candidate, ORB-15 (tier 2)
- [x] `vetting.rs` types: `Verdict { Approve, Reduce(Factor), Reject }` (tier 1, types only; parsing comes in P4)
- [x] `risk.rs`: `Limits::for_mode`, `gate()` for every §9 rule, sizing, open-risk budget, clusters, `self_test()`; property tests (tier 1)
- [x] `engine.rs`: `State`, `Event`, `Effect`, pure `step()`; reservations; one trade per symbol; flatten timing (tier 1)
- [ ] `backtest.rs`: replay feeder + simulated broker (§13 fill model), trades → metrics (tier 2)
- [ ] Research stats: day-block bootstrap, random-entry baseline, walk-forward split, trial log (tier 2)
- [ ] `history.rs`: Alpaca bars `feed=iex`, throttled, resumable cache in `data/`; tested on saved responses (tier 2)
- [ ] CLI: `fetch`, `backtest` (tier 3)
- [ ] ⛔ Download history (needs keys) and run the P1 gate (§13) → owner decides: go to P2, iterate, or stop

## P2 — Safety core
- [ ] Broker orders: bracket submit (idempotent client_order_id), cancel, `close_all`, OCO for partial fills, typed Order/Position (tier 1)
- [ ] Kill switch at `$BMV_HOME/data/STOP`, reconcile from Alpaca, startup self-tests (tier 1)
- [ ] `cargo-mutants` on `risk.rs`: no surviving mutants in limit checks
- [ ] ⛔ Paper tests that resolve every [verify] in DESIGN

## P3 — Paper engine
- [ ] `market_data.rs` websocket + backfill + staleness; tokio event loop around `step()`; calendar/clock; flatten backstop; `status`, `report` (tier 1/2)
- [ ] ⛔ 10 unattended paper days; live candidates identical to replay

## P4 — AI layer
- [ ] `llm.rs`, `vetting.rs` parsing, prompt versioning, regression set, drift alarm; second paper account shadow; read-only web dashboard (tier 1/2)
- [ ] ⛔ 20+ paper days; pre-registered Claude-vs-shadow gate

## P5 — Live pilot
- [ ] ⛔ Mac mini, phone approvals, live limits, 30 live days
