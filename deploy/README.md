# Flatten backstop (launchd)

If the engine dies, hangs or the Mac sleeps, Alpaca still holds every stop and target. This job also
closes anything left near the close. It closes everything in the account, including any position
you opened by hand. It runs `bmv-trader backstop` every 2 minutes:

- Between closes it exits at once, with no network call and no Keychain read.
- From 3 minutes before today's close (13:00 or 16:00 ET, taken from Alpaca's calendar) until 1
  minute after it, it checks positions and orders. If anything is open, it flattens and logs the
  result to `data/backstop.log`.

The engine flattens at 5 minutes before the close, so on a normal day the backstop finds the account
flat and does nothing.

## Install (you run these; nothing installs itself)

```sh
cd ~/Documents/projects/bmv/bmv-trader
cargo build --release
cp deploy/com.bmv-trader.backstop.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.bmv-trader.backstop.plist
```

## Check it works

```sh
launchctl kickstart -k gui/$(id -u)/com.bmv-trader.backstop   # run once now
launchctl print gui/$(id -u)/com.bmv-trader.backstop | grep -E "state|last exit"
cat data/backstop.log
```

`last exit code = 0` and an empty log away from a close means it works. Near a close, the log shows
`flat`. The first run near a close may ask for Keychain access: choose **Always Allow**.

**If the log says "Operation not permitted":** macOS protects `~/Documents` from background jobs.
Either give the binary Full Disk Access (System Settings → Privacy & Security → Full Disk Access →
`+` → `target/release/bmv-trader`), or move the project out of `~/Documents` and update the paths in
the plist. Full Disk Access is tied to that exact file, so after a rebuild check the log again.

## Remove

```sh
launchctl bootout gui/$(id -u)/com.bmv-trader.backstop
rm ~/Library/LaunchAgents/com.bmv-trader.backstop.plist
```

# Trial 10 nightly analyst (launchd)

`bmv-trader desk-run` runs on weekdays at 16:30 Mac time (17:30 ET on US Central time). It downloads
the last ~420 days of adjusted daily bars, finds trial 9's signals on today's close, asks the
TradingAgents analyst (Claude only) about the top ones within the $100/month budget, and scores older
signals. It **never places an order**.

Before installing:

```sh
# the analyst's only key; it is passed to the sidecar and nothing else is
security add-generic-password -s bmv-trader -a ANTHROPIC_API_KEY -w
# only if Anthropic says the key "is not scoped to a workspace": the workspace ID (wrkspc_...)
security add-generic-password -s bmv-trader -a ANTHROPIC_WORKSPACE_ID -w
# one manual run first, to measure what an analysis costs (counts toward the budget)
mkdir -p data/desk
target/release/bmv-trader desk-analyse --symbols AAPL --date 2026-11-03
```

Install, check and remove work like the backstop above, with `com.bmv-trader.desk.plist`. The log is
`data/desk/run.log`; `bmv-trader desk-report` shows progress at any time.

# Tier 2: local Ollama through OmniRoute (DECISION A13, S22)

Low-risk notes for you (desk brief, analyst digest, session summary, Sunday digest) come from a
local model. Nothing leaves the Mac, and no trading code reads `data/notes/`.

One-time setup (you run these):

```sh
# 0. the OmniRoute password pasted into chat earlier is exposed: change it in the dashboard first
# 1. local model, bound to 127.0.0.1 (Ollama's default)
brew install ollama
brew services start ollama
ollama pull qwen3:8b
# 2. in the OmniRoute dashboard (omniroute dashboard):
#    - add provider "Ollama" at http://127.0.0.1:11434
#    - create a combo named  bmv-local  with only the qwen3:8b model
#    - create an API key named bmv-tier2, and note its ID
# 3. hand bmv-trader the key and its ID (each prompts for the value)
security add-generic-password -s bmv-trader -a OMNIROUTE_API_KEY -w
security add-generic-password -s bmv-trader -a OMNIROUTE_KEY_ID -w
# 4. check, then rotate once by hand
target/release/bmv-trader llm-health
target/release/bmv-trader llm-rotate
```

Install the two jobs (times are Mac local time; this Mac is on US Central, one hour behind ET):

```sh
cargo build --release          # the jobs run target/release/bmv-trader
mkdir -p data/notes            # the notes job logs here
cp deploy/com.bmv-trader.llm.plist deploy/com.bmv-trader.notes.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.bmv-trader.llm.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.bmv-trader.notes.plist
```

- `llm` runs daily at 05:30: it rotates the key, then checks health (`data/llm_health.json`, log
  `data/llm.log`).
- `notes` runs daily at 18:15 and writes any due notes. `desk-run` also tries right after it
  finishes.
- The rotation regenerates the key and stores it in the Keychain over stdin, so it never appears in a
  process list. It then applies the key's policy (only `bmv-local`, $0 a day, 30 requests a minute)
  before a test call.
- If a step fails, the error and the exact manual fix are kept in `data/llm_rotate_error.json`, and
  health shows FAIL until a rotation succeeds.
- **On the first manual `llm-rotate`, watch the output.** The regenerate output format and the policy
  flags are checked for real only then.
- Notes written while Tier 2 is down are saved as `*.facts.md`, and a later run replaces them with the
  worded note.
