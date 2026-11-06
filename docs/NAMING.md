# Naming conventions

One vocabulary for every name in bmv-trader: code, commands, files, OS identities, keys and records.
Owner's decision S44 (2026-11-05). `tests/naming.rs` checks the parts that can be checked, and
`.claude/rules/naming.md` points every session here.

## Principles

1. **One system prefix, `bmv`.** Anything that leaves the repo carries it: OS users, paths, launchd
   labels, Keychain services, environment variables, and keys made at providers. It is never used
   inside the code, where the crate already scopes names.
2. **Case follows the audience:**
   - **Things a person types, or the OS shows,** are lowercase-kebab: commands, flags, scripts,
     paths, labels and ids.
   - **Things a program reads** are snake_case: JSON fields, data file names, Rust identifiers.
   - **Environment variables** are UPPER_SNAKE.
3. **Name the thing, not how it works:** `free-keys`, not `openrouter-provisioner`. Use one word
   per idea, and the same word everywhere (see the vocabulary).
4. **No new abbreviations.** Only those in the vocabulary below are allowed.
5. **Installed identities are never renamed in place.** OS users, Keychain items, launchd labels and
   provider-side keys only change through a migration script that creates the new name, moves the
   data, and then removes the old one. Until then, they're listed under "Deviations kept".
6. **Dates in names** are `YYYY-MM-DD`, `YYYY-MM`, or `YYYYMMDD-HHMM` when a key is made more than
   once a day. Times in records are RFC 3339 in UTC; times shown to a person are New York time.

## Vocabulary

| Word | Means | Never |
|---|---|---|
| `engine` | the `bmv-trader` binary: data, backtests, risk, orders | core, app |
| `desk` | the Quant Desk crate and its daemon: the authority for strategies, signatures, budgets | server, api |
| `clip` | Paperclip, the third-party scheduler (`bmvclip`, `bmv-clip`, `clip-*.sh`) | pc, paper |
| `agent` | a desk agent (one role, one id, one charter) | bot, worker |
| `route` | an OmniRoute combo (`bmv-free`) | combo (in our docs), pool |
| `connection` | an OmniRoute provider connection (`openrouter-free`) | provider (that's the company) |
| `key` | a secret used to call a service | token, which is kept for the desk's per-agent logins |
| `token` | a desk agent's login to the desk | key |
| `trial` | a pre-registered experiment with a verdict (trial 10, trial 12) | test, run |
| `run` | one execution of a job | trial |
| `job` | a scheduled launchd task | cron, task |
| `notes` | Tier-2 text for the owner (briefs, digests) | reports |
| `console` | the local web page `bmv-trader ui` serves (Overview, Desk, Services, Help) | dashboard, admin; Anthropic's site is always "Anthropic Console" |
| `contract` | one listed option (S48), named by its OCC symbol; quantities are whole contracts | option contract, option (Rust's `Option` is taken) |
| `premium` | a contract's price; premium × 100 × contracts is the dollars paid and the most a bought contract can lose | price, which stays the stock's price |
| `call`, `put` | which right a contract gives, the enum `Right::{Call, Put}` | long/short; a model or HTTP request is a `request` in contract code |
| `expiry` | the date a contract ends | expiration, maturity |
| `strike` | the price a contract's right is at | strike_price |
| `insider` | a company's Section 16 reporting owner (officer, director, 10% holder) who files Form 4 | the desk's own agents or staff |
| `filing` | one document filed with the SEC, dated by its filing date | report, which is the owner's 30 November report |

Allowed abbreviations:
- `hod` (Head of Desk), `res` (Researcher), `val` (Validator), `stress` (Stress Lead)
- `llm` (the engine's model module)
- `mcp`, `api`, `ui` (the standard meanings)
- `occ` (the Options Clearing Corporation's contract symbol, `OccSymbol`)
- `sec` (the US Securities and Exchange Commission; `data/sec_contact`)

## By kind

| Kind | Pattern | Examples |
|---|---|---|
| **Rust crate** | lowercase-kebab package | `bmv-trader`, `desk` |
| **Rust module** | one snake_case noun for what it owns | `free_keys`, `keys`, `risk`, `agent` |
| **Rust items** | Rust API guidelines: `UpperCamel` types, `snake_case` functions and variables, `SCREAMING_SNAKE` constants | `Spend`, `month_spend`, `TIER2_MODEL` |
| **Rust tests** | the behaviour as a sentence, in snake_case | `a_call_that_fails_after_sending_stays_booked_at_the_worst_case` |
| **Binary** | `<crate>` or `<crate>-<role>` | `desk`, `desk-agent`, `desk-mcp`, `desk-sign` |
| **Engine subcommand** | `<area>` or `<area>-<verb>`, kebab; `up` is the one bare verb, for bringing the whole system up | `notes`, `keys`, `free-keys`, `desk-run`, `llm-rotate`, `up` |
| **Flags** | `--kebab`; values after the flag, never bare positional words | `--record openrouter-management`, `--renew` |
| **Script** | `scripts/<area>-<verb>.sh`; one-area entry points may be `<area>.sh`; sourced helpers are nouns | `clip-setup.sh`, `free-key-add.sh`, `keys.sh`, `hidden-user.sh` |
| **Scripts run with sudo** | end in `-setup.sh` (or close a setup step, like `clip-close-signup.sh`), and show the `sudo` command in the header comment | `desk-setup.sh`, `clip-agents-setup.sh` |
| **Owner's launchd job** | label `com.bmv-trader.<job>`, one lowercase word; file `deploy/<label>.plist` | `com.bmv-trader.keys`, `com.bmv-trader.freekeys` |
| **System daemon** | label `com.bmv.<component>[-<part>]`, written by its `-setup.sh` | `com.bmv.desk`, `com.bmv.clip`, `com.bmv.clip-db` |
| **macOS user** | `bmv<component>` or `bmv<agent>`, no separator, at most 12 characters | `bmvdesk`, `bmvclip`, `bmvval`, `bmvhod` |
| **Installed paths** | binaries `/usr/local/libexec/bmv-<component>`, config `/usr/local/etc/bmv-<component>`, state `/usr/local/var/bmv-<component>`, logs `/usr/local/var/log/bmv-<component>`; an agent user's own folder `/usr/local/var/<user>` | `/usr/local/var/bmv-clip`, `/usr/local/var/bmvval` |
| **Repo config** | `deploy/<noun>.json`, snake_case fields | `deploy/models.json`, `deploy/agents.json` |
| **Data files** | `data/<area>[_<noun>].<ext>`, snake_case; a folder per area once it has more than one file; downloaded originals keep their source's file names | `data/tier2_spend.jsonl`, `data/desk/analyses.jsonl`, `data/insider/2024q1_form345.zip` |
| **Data file kinds** | `.jsonl` append-only log; `.json` current state; `.log` a job's output; `.lock` a lock | `free_keys.jsonl`, `keys.json`, `keys.log`, `free_keys.lock` |
| **Keychain service** | `bmv-trader` for the engine and owner; `bmv-desk-models` for the desk agents' model keys; `bmv-desk` for desk tokens the owner's own programs hold | |
| **Keychain account** | under `bmv-trader`: UPPER_SNAKE, the name it would have as an environment variable (`<PROVIDER>_<PURPOSE>_KEY`); under `bmv-desk-models`: `<provider>-<scope>` kebab; under `bmv-desk`: the desk agent id | `OPENROUTER_MANAGEMENT_KEY`; `anthropic-desk`; `engine-1`, `console-1` |
| **Key at a provider** | `bmv-<use>-<YYYY-MM>`, or `bmv-<use>-<YYYYMMDD-HHMM>` when made more than once a month | `bmv-engine-2026-12`, `bmv-free-20261105-1820` |
| **Key tracker id** | `<provider>-<scope>` kebab | `claude-engine`, `openrouter-management` |
| **Environment variable** | `BMV_<NOUN>`; a third party's own prefix is left alone | `BMV_HOME`, `BMV_NEW_KEY`; `PAPERCLIP_RUN_ID` |
| **Desk agent id** | `<role abbreviation>-<n>`; a program's own login is `<program>-<n>` | `hod-1`, `res-1`, `val-1`, `stress-1`; `engine-1`, `console-1` |
| **Console route** | pages `/<page>` and their script `/<page>.js`; data `/api/<area>` or `/api/<area>/<noun>`, kebab segments | `/desk`, `/desk.js`, `/api/keys`, `/api/desk/snapshot` |
| **Paperclip agent name** | `<Role Name> (<id>)` | `Validator (val-1)` |
| **OmniRoute route** | `bmv-<tier>` | `bmv-free` |
| **OmniRoute connection** | `<provider>-<tier>` | `openrouter-free` |
| **Model ids** | the provider's exact id, never an alias | `claude-sonnet-5-5`, `nvidia/nemotron-3-ultra-550b-a55b:free` |
| **Decision ids** | `S` (system), `A` (analyst), `R` (research) plus the next number, never reused | `S44`, `A12`, `R12` |
| **Commit subject** | `<area or decision>: <what changed>` | `Quant Desk S43: every key in one place` |

## Deviations kept

These names break a rule above. Each one is installed on the owner's Mac or is a third party's
name, so it stays until a migration script replaces it.

| Name | Rule it breaks | Why it stays |
|---|---|---|
| Keychain accounts `ANTHROPIC_API_KEY`, `OMNIROUTE_API_KEY` | none; they follow the `bmv-trader` rule | listed so the two account styles aren't "fixed" by mistake |
| `deploy/paperclip/` | uses the product's name, not `clip` | it's Paperclip's own package lock, named for the package |
| `scripts/desk-wave1.sh` | names a rollout phase, not an action | its commands (`agents`, `tokens`, `budgets`) are in the owner's runbook; rename it when wave 2 changes it |
| `com.bmv-trader.llm` runs `llm-daily` | the label names the job, the command its action | that's the rule: labels name jobs |
| Console route `/api/desk` | it serves trial 10's progress, but `desk` now means the Quant Desk | the Overview page reads it; rename when trial 10's view is next changed |
| `scripts/*.py` | Rust-first (`.claude/rules/rust-first.md`) | ported when next changed |

## Adding a name

1. Find its kind in the table above and follow the pattern.
2. If no row fits, add one here, in the same change, with an example.
3. A word that isn't in the vocabulary is added there first, with what it must never be confused with.
4. `CARGO_TARGET_DIR=target/lessons scripts/check.sh` runs `tests/naming.rs`. A name it rejects is
   renamed, not exempted.
