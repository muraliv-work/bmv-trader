# Decision log

The key decisions behind bmv-trader: what was chosen, why, and what would make us revisit it.
Details live in [DESIGN.md](DESIGN.md); this file is the short "why". Add a row when a decision
is made or changed. Never delete an old one: mark it **Superseded** and link the replacement.

Status: **Active** · **Pending** (needs the owner or data) · **Superseded**

## Platform

| # | Decision | Why | Revisit when | Status |
|---|---|---|---|---|
| D1 | **Alpaca**, free plan (IEX data) | Free paper trading with the same API as live; commission-free; $0 data budget | IEX gaps hurt results in P3/P5 → consider paid SIP data | Active |
| D2 | **Rust**, single binary | Owner's language; types enforce money and safety rules at compile time | Never for the core | Active |
| D3 | **Runs on the laptop first, then a Mac mini** (launchd), cloud VM optional | No hosting cost until the system makes money | After P5 if the live pilot is profitable | Active |
| D4 | **US stocks/ETFs, regular hours only** | Liquid, simple session rules, no overnight risk | Crypto (24/7) is a separate P6 decision | Active |
| D5 | **Integer cents, whole shares** | No float rounding in money; simpler order math | Fractional shares or crypto quantities (P6) | Active |

## Safety

| # | Decision | Why | Revisit when | Status |
|---|---|---|---|---|
| S1 | **Paper by default; live needs `TRADING_MODE=live` + the confirm phrase + separate live keys, all in `.env`** | A live order must never happen by accident; shell variables can't override `.env` | Never | Active |
| S2 | **Keys only in `.env` (mode 600), never committed or printed** | Keys can move real money | Never | **Superseded by S13** for where keys live; never committed or printed still holds |
| S3 | **"Unknown means stop"**: any state we can't confirm blocks new entries | A wrong guess about orders or positions costs money; waiting costs nothing | Never | Active |
| S4 | **The broker is the source of truth**; the journal is only an audit log | Our own records can drift from reality; Alpaca's can't | Never | Active |
| S5 | **Only `risk::gate()` can create an `OrderIntent`**, and only an `OrderIntent` can open a position | One place enforces every limit, so nothing can skip it | Never | Active |
| S6 | **Every entry is a bracket order** (stop + target held by Alpaca) | Positions stay protected if the Mac sleeps or the engine crashes | Never | Active |
| S7 | **Conservative limits**: 1% risk per trade, −2% daily halt, 3 losses in a row halts, ≤ 5 positions (live pilot: tighter) | Survive long enough to learn whether there's an edge | After 30 profitable live days | Active |
| S8 | **Flat 5 minutes before the close**, plus a separate backstop job at close − 3 min | Day trading only; no overnight gaps | Never in this system | Active |
| S10 | **An approved order is sent once**: `Bracket` consumes the `OrderIntent`; an unknown outcome hands it back for a retry with the **same** id only | A retry under a new id could double a position without a second gate pass | Never | Active |
| S11 | **A refused OCO means close that position now** (`close_position`); "not found" right after an unknown submit is not proof of absence | A partial fill must never sit without a stop | Never | Active |
| S12 | **STOP present at startup means no trading today**; any error reading it counts as engaged | Unknown means stop | If an engine start is ever needed just to flatten | Active |
| S13 | **Keys come from the macOS Keychain** (service `bmv-trader`); paper keys may also sit in `.env`, **live keys never may**; a key in both places is refused; a Keychain error stops startup (no fallback to `.env`) | No plain-text live keys on disk; one source per key so they can't disagree | Moving to a cloud VM (D3) → its secret manager | Active |
| S14 | **No mid-day restart**: if the journal shows a run since today's open, `run` refuses until tomorrow (after `flatten`) | A restart would forget today's trades, losses and used symbols, so limits could be exceeded | When engine state can be rebuilt from Alpaca's fills | Active |
| S15 | **Paper run loop v1 polls REST** once a minute, blocking, no tokio | Smallest working loop with no new dependencies | Websocket upgrade later in P3 | Active |
| S17 | **One `run` at a time**: `run` holds an OS lock on `data/run.lock` for its lifetime | Two engines on one account would each think they own every position | Never | Active |
| S18 | **Flatten backstop = launchd every 2 min, `bmv-trader backstop`**. It exits at once except within 3 min before to 1 min after a 13:00/16:00 ET close (later, market orders would queue for the next open); then the calendar decides, and anything open is flattened. It never engages STOP (that would block tomorrow's start) | Covers an engine that died, hung or slept, using the same `flatten` as the CLI. No network or Keychain the rest of the day | When the engine runs on an always-on host | Active |
| S19 | **Swing book beside the intraday book**: `bmv-s-` ids, disjoint symbols, GTC protective stops at Alpaca. The end-of-day flatten and backstop close only the intraday book; operator STOP/flatten close everything | Owner chose 2–10 day swing (2026-11-03). Agents work in minutes, and a swing hold must survive the close and the backstop | If the books share symbols, or when going live | Planned |
| S20 | **Sidecars (OpenBB, TradingAgents, paperclip) never see broker keys.** Loopback only, scrubbed environment, pinned public releases (never `REFERENCE-REPOS`). Their output is strictly parsed proposals that can only shrink or veto a code candidate | Every agent inherits its environment, so credential reach is the real risk; rules belong in code, not prompts | Never | Planned |
| S21 | **Desk roles** (DESIGN §4b): Chief of Staff, Scout, Analyst and Risk Reviewer are agents. Chart Analyst, Strategist, Risk Officer and Trader are Rust code. Only the Rust engine places orders | The owner's reference desk, with its authority moved into code | When a role earns more authority on the ladder | Planned |
| S9 | **No buy/sell buttons in any UI**; only STOP and flatten | Humans don't place trades around the gate | Never | Active |
| S16 | **Web console (`ui`) on 127.0.0.1 only**, with a per-launch token in the URL fragment, an exact Host check, a same-Origin check on POST, a strict CSP and no CORS. It shows what `watch` shows, and its only controls are STOP and flatten, each needing a typed word that the server checks too. Flatten engages STOP first, and still flattens if STOP can't be written. Alpaca is read on its own thread and each connection gets one (with a 5 s request deadline), so neither a slow Alpaca nor a slow client delays STOP. `run` writes `data/heartbeat` every tick, so the console shows "engine not running" instead of a stale "trading". There is no way to clear STOP or start a run from it. The token stays in browser history and terminal scrollback until `ui` exits | The UI can only make things safer or show them; anything that adds risk needs the terminal. A separate process never touches the engine, and STOP is the existing channel. No new dependencies (std HTTP) | Tailscale or phone access is its own decision; a new control means a new decision | Active |

## AI

| # | Decision | Why | Revisit when | Status |
|---|---|---|---|---|
| A1 | **Rules find trades; Claude vets them** (approve / reduce 1–99% / reject) | Rules can be backtested honestly; an LLM can't (it already knows past outcomes) | Only via the authority ladder (DESIGN §10), earned in paper | Active |
| A2 | **Claude can only shrink or veto a trade**, never create one or change its prices | Saying "no" is always safe; saying "buy" untested is not | L4 in the authority ladder, if ever | Active |
| A3 | **Any Claude failure = reject** (timeout, bad JSON, error); no fallback model | A guess is worse than skipping one trade | Never | Active |
| A4 | **Claude is called directly, never through OmniRoute** | No silent provider or model swaps in the decision path | Never | Active |
| A5 | **Free models (OmniRoute) never touch decisions**: summaries, digests, logs, research ideas only | Unmeasured quality, provider rotation, and prompt injection through summaries | Never for decisions | Active |
| A6 | **Claude reads raw headlines, not summaries from other models** | A summarizer can launder an injected instruction into trusted-looking text | Never | Active |
| A7 | **AI does not pick the strategy each day**; strategy selection uses backtested rules (e.g. the trend filter) | AI picks can't be backtested; rule-based switching can | If an AI selector beats rules in a pre-registered paper test | Active |
| A8 | **Claude must beat a shadow account** that runs the same rules without Claude (P4 gate) | Otherwise Claude is cost and risk without benefit | — | Pending (P4) |
| A9 | **Local Ollama (via OmniRoute) is the preferred free tier for non-decision tasks** | Free, private, no rate limits | When P4 starts | Pending |
| A10 | **Two LLM tiers, fixed per task in code**: Tier 1 Claude direct for anything that can change a trade or strategy; Tier 2 OmniRoute (Ollama first) for chores describing facts the code computed. No fallback across tiers; proposals need Tier-1 provenance | Long-running agents cost too much on Claude; free models must stay out of decisions (A4, A5) | If a free model passes its own measured test | Planned |
| A11 | **Claude budget $100/month**, enforced by a cost ledger; daily `llm-health` checks routes, OAuth expiry and budget. No automatic key rotation (manual, quarterly) | Owner's cap; automated rotation risks locking the desk out | When the budget changes | Planned |

## Strategy and research

| # | Decision | Why | Revisit when | Status |
|---|---|---|---|---|
| R1 | **Strategy v1 = ORB-15 long** on 10 liquid symbols | Simple, well known, few parameters, easy to test | If it fails the P1 gate | **Failed P1 (trial 1, 2026-11-03):** 1,555 trades, −0.105R, CI −0.18..−0.03R, PF 0.86, 22/61 windows; beats random (−0.25R) |
| R2 | **Trend filter is an optional, separate trial** (`--trend`): trade only while SPY and the symbol are above today's VWAP and open | Breakouts tend to work better in a rising market; testable with today's bars only | If it fails the P1 gate | **Failed P1 (trial 2, 2026-11-03):** 970 trades, −0.120R, CI −0.23..−0.00R, PF 0.84, 26/61 windows; beats random (−0.25R) |
| R2b | **Trend filter needs fresh SPY data** (newest SPY bar ≤ 120 s old), or no trade | Stale market data would let the filter fail open | If MARKET becomes a thinner symbol | Active |
| R2c | **Research data = IEX 2020-08-03 → 2025-12-31**; holdout pinned from 2025-10-02; missing days only via explicit `--allow-gaps` (one so far: 2025-03-10, all symbols) | Free IEX minute history starts late July 2020; silent gaps would bias results | Paid SIP data | Active |
| R2d | **Marketable entry (`--marketable`, limit 5 bps above the close) is the only entry variant tried** | Tests whether resting limits only filled failing breakouts | — | **Failed P1 (trial 3, 2026-11-03):** 2,296 trades, −0.117R, CI −0.21..−0.02R, PF 0.86, 18/61 windows. Hypothesis rejected: ORB has no edge here after costs. **Stop tuning ORB**; next strategy is a new family |
| R2g | **Trial 6: gap-and-go on scanned gappers** (DESIGN §13b, pre-registered: all US stocks incl. delisted, $5+, $1M IEX dollar volume, +2..30% gap, top 10 a day; trial 4's rules) | Trial 4 was starved of gap days | — | **Failed P1 (trial 6, 2026-11-03):** 1,736 trades, −0.256R, CI −0.35..−0.17R, PF 0.69, 11/60 windows, DD 444R. Beats random (−0.44R) but clearly loses after costs. **Gap-and-go is closed**; with R1–R2f, six intraday long rules have failed |
| R2h | **Trial 7: 12-1 month momentum** (DESIGN §13c, pre-registered: adjusted SIP daily bars 2016–2026, all US stocks incl. delisted, $20M dollar volume, top 20 monthly, 0.2% per side) | Intraday rules failed; momentum has the longest record and low turnover | — | **Failed P1 (trial 7, 2026-11-03):** 104 months 2017-02..2025-09. 1.49%/month, CAGR 9.9%, Sharpe 0.43, max DD 54.6%, against random 20 at 4.0% CAGR and SPY at 14.9% CAGR, Sharpe 0.92, DD 23.3%. Excess over random CI −1.12..+3.04%/month (not significant). Beat random in 6/9 years but **lost to simply holding SPY** with twice its drawdown. Most-held names were volatile small caps and crypto miners (VKTX, BTU, CVNA, RIOT, MARA). **Plain momentum is closed**; the holdout stays unspent |
| R2i | **Trial 8: the Desk Signal swing strategy** (DESIGN §7d, pre-registered: EMA 8/21/50/200 stack, 20-day-high breakout, volume 1.5×, RSI > 50, ATR below its 90th percentile; 2×ATR stop, 3×ATR target, 10-session time stop; trial 7's universe and costs) | The owner's reference desk uses this signal; the swing book needs a proven strategy before any order | — | Pre-registered, not run |
| R3 | **Indicators use today's regular-hours bars only** (no premarket, no prior day) | Simple and avoids data-alignment bugs | Multi-day trend filters need prior-day data → new decision | Active; one exception: R2e (yesterday's close, backtest only) |
| R2e | **Trial 4: gap-and-go** (DESIGN §7b, pre-registered, ≥ +1% gap, 5-min range breakout, gap still open) | Gap days carry news and momentum the ORB missed | — | **Failed P1 (trial 4, 2026-11-03):** 385 trades, −0.036R, CI −0.23..+0.16R, PF 0.95, 24/61 windows, DD 30R. Best so far, but indistinguishable from zero; beats random |
| R2f | **Trial 5: VWAP pullback** (DESIGN §7c, pre-registered, from 10:00, first resumption above VWAP after a touch) | Trend continuation on a pullback, not a breakout | — | **Failed P1 (trial 5, 2026-11-03):** 2,521 trades, −0.219R, CI −0.30..−0.13R, PF 0.73, 12/61 windows, DD 561R. **No better than random entries** (−0.25R): the signal adds nothing |
| R4 | **Random-entry baseline**: same exits and sizing, random entry time | If a strategy can't beat random entries, the signal adds nothing | Never | Active |
| R5 | **Pessimistic fill model** (slippage, partial fills, stop wins ties, no fill after TTL) | Backtests that flatter you lose real money | After paper trading measures real slippage | Active |
| R6 | **Every backtest run is a logged trial; the significance bar rises with each trial** (Bonferroni) | Stops "try 50 variants until one looks lucky" | Never | Active |
| R7 | **Final holdout (last 63 sessions) is pinned and evaluated once** | A holdout used twice is no longer a test | Never | Active |
| R8 | **P1 gate** must pass before P2 work goes live: ≥ 100 trades, CI > 0, beats random, ≥ 60% profitable windows, PF > 1.2, DD < 10R, holdout > 0 | Only build on an edge that survives honest testing | — | **Not passed** by trials 1–7 (ORB ×3, gap-and-go ×2, VWAP pullback, monthly momentum); owner decides next |

## Development

| # | Decision | Why | Revisit when | Status |
|---|---|---|---|---|
| V1 | **Testing by tier**: money-critical code gets tests first, proptests and `cargo-mutants`; the rest lighter | Spend effort where bugs cost money | — | Active |
| V2 | **Tiered `/review`**: tier 1 = rust + code + security / silent-failure / type-design as relevant; tier 2 = rust + code; docs = none | Review cost proportional to risk | — | Active |
| V3 | **Development with Claude; low-risk dev tasks may use free models** | Saves tokens where mistakes are cheap and caught by tests | — | Active |
| V4 | **Nothing is pushed without the owner's approval** | The owner controls what leaves the machine | Never | Active |

## Open (owner to decide)

From DESIGN §17: crypto in P6 or dropped · shorts after longs validate · paid SIP data · phone
approval channel (Telegram or ntfy) · the larger rule-based universe for research.
