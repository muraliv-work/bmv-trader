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
| S2 | **Keys only in `.env` (mode 600), never committed or printed** | Keys can move real money | Never | Active |
| S3 | **"Unknown means stop"**: any state we can't confirm blocks new entries | A wrong guess about orders or positions costs money; waiting costs nothing | Never | Active |
| S4 | **The broker is the source of truth**; the journal is only an audit log | Our own records can drift from reality; Alpaca's can't | Never | Active |
| S5 | **Only `risk::gate()` can create an `OrderIntent`**, and only an `OrderIntent` can open a position | One place enforces every limit, so nothing can skip it | Never | Active |
| S6 | **Every entry is a bracket order** (stop + target held by Alpaca) | Positions stay protected if the Mac sleeps or the engine crashes | Never | Active |
| S7 | **Conservative limits**: 1% risk per trade, −2% daily halt, 3 losses in a row halts, ≤ 5 positions (live pilot: tighter) | Survive long enough to learn whether there's an edge | After 30 profitable live days | Active |
| S8 | **Flat 5 minutes before the close**, plus a separate backstop job at close − 3 min | Day trading only; no overnight gaps | Never in this system | Active |
| S10 | **An approved order is sent once**: `Bracket` consumes the `OrderIntent`; an unknown outcome hands it back for a retry with the **same** id only | A retry under a new id could double a position without a second gate pass | Never | Active |
| S11 | **A refused OCO means close that position now** (`close_position`); "not found" right after an unknown submit is not proof of absence | A partial fill must never sit without a stop | Never | Active |
| S12 | **STOP present at startup means no trading today**; any error reading it counts as engaged | Unknown means stop | If an engine start is ever needed just to flatten | Active |
| S9 | **No buy/sell buttons in any UI**; only STOP and flatten | Humans don't place trades around the gate | Never | Active |

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

## Strategy and research

| # | Decision | Why | Revisit when | Status |
|---|---|---|---|---|
| R1 | **Strategy v1 = ORB-15 long** on 10 liquid symbols | Simple, well known, few parameters, easy to test | If it fails the P1 gate | Active, untested on real data |
| R2 | **Trend filter is an optional, separate trial** (`--trend`): trade only while SPY and the symbol are above today's VWAP and open | Breakouts tend to work better in a rising market; testable with today's bars only | If it fails the P1 gate | Active, untested on real data |
| R2b | **Trend filter needs fresh SPY data** (newest SPY bar ≤ 120 s old), or no trade | Stale market data would let the filter fail open | If MARKET becomes a thinner symbol | Active |
| R3 | **Indicators use today's regular-hours bars only** (no premarket, no prior day) | Simple and avoids data-alignment bugs | Multi-day trend filters need prior-day data → new decision | Active |
| R4 | **Random-entry baseline**: same exits and sizing, random entry time | If a strategy can't beat random entries, the signal adds nothing | Never | Active |
| R5 | **Pessimistic fill model** (slippage, partial fills, stop wins ties, no fill after TTL) | Backtests that flatter you lose real money | After paper trading measures real slippage | Active |
| R6 | **Every backtest run is a logged trial; the significance bar rises with each trial** (Bonferroni) | Stops "try 50 variants until one looks lucky" | Never | Active |
| R7 | **Final holdout (last 63 sessions) is pinned and evaluated once** | A holdout used twice is no longer a test | Never | Active |
| R8 | **P1 gate** must pass before P2 work goes live: ≥ 100 trades, CI > 0, beats random, ≥ 60% profitable windows, PF > 1.2, DD < 10R, holdout > 0 | Only build on an edge that survives honest testing | — | **Pending (needs keys + data)** |

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
