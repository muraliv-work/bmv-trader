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

**Pinned binary (R13 step 0b, 2026-11-06).** Trial 10 runs a pinned copy, never `target/release`, so
rebuilds can't change it. `bmv-trader desk-ping desk-run` runs it, then pings the healthchecks.io
check `desk-run` (or `desk-run/fail`, with the run's output) with the Keychain item `bmv-trader` /
`HEALTHCHECKS_PING_KEY`. launchd starts `target/release/bmv-trader`, never a script: macOS won't let
launchd run bash scripts in `~/Documents` ("Operation not permitted", exit 126), even with Full Disk
Access for `/bin/bash`, while the binary and its children can read the project:

```sh
sudo mkdir -p /usr/local/libexec
sudo cp target/release/bmv-trader /usr/local/libexec/bmv-trial10
shasum -a 256 /usr/local/libexec/bmv-trial10   # must be ecf992de...0bd7c (docs/REPORT-2026-11.md)
security add-generic-password -s bmv-trader -a HEALTHCHECKS_PING_KEY -w   # healthchecks.io project ping key
cp deploy/com.bmv-trader.desk.plist ~/Library/LaunchAgents/
launchctl bootout gui/$(id -u)/com.bmv-trader.desk; launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.bmv-trader.desk.plist
```

# Reconciliation every 15 minutes (launchd, R13 step 3b)

`bmv-trader desk-ping reconcile` runs `bmv-trader desk-reconcile` from `target/release` at 8:00–14:45 Mac
time (9:00–15:45 ET), every day, market open or not (positions and GTC stops live at any hour). It
checks that the account holds only the drills in `data/drill.json`, each with one working,
standalone GTC sell stop for its full qty, and no other order. A mismatch, or an account it can't
read, is an operational halt (step 3c, below). Then it pings the healthchecks.io check `reconcile`, or `reconcile/fail` on a
mismatch or an error, with the run's output as the ping body, so the email says what was wrong.
The log is `data/reconcile.log`.

The `reconcile` check on healthchecks.io: schedule `*/15 8-14 * * 1-5`, time zone
`America/Chicago`, grace 10 minutes. Weekend pings are harmless. The Mac must stay on US Central
time; the plist's hours are Mac time.

```sh
cargo build --release      # trial 10 has its pinned copy; trial 12 runs from target/lessons
cp deploy/com.bmv-trader.reconcile.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.bmv-trader.reconcile.plist
launchctl kickstart -k gui/$(id -u)/com.bmv-trader.reconcile   # run once now
tail -3 data/reconcile.log
```

Then un-pause the `reconcile` check on healthchecks.io and confirm its page shows that ping: a
wrong key or slug only shows up as a missed ping.

**The operational halt (R13 step 3c).** On a mismatch the run engages `data/STOP` (reason
`reconcile: ...`), cancels every unfilled entry order by Alpaca's id, yours too (anything but a sell on a symbol held
long, or a buy on one held short), and keeps every order that protects a position, so the drill's stop stays. The log and the
fail email give the halt's time, from the first cancel to the last re-read: `halt in N ms` (at most
5000), `HALT TOO SLOW` or `HALT INCOMPLETE`. A partly filled order is never cancelled (cancelling a
bracket parent could take its stop): the halt names it and you deal with it by hand.
No position is closed. Nothing trades until you check the account and `rm data/STOP`; while the
mismatch is there, every run halts again. Trial 10's `desk-run` doesn't read STOP, so it goes on.

**The clean-session count (R13 step 3e).** Every run also appends a line to
`data/reconcile/runs.jsonl`. `bmv-trader desk-report` shows how many NYSE sessions in a row were
clean, out of the 10 Level A needs. A session is clean when every 15-minute slot from 9:00 ET to its
close ran, every run held a drill position, and every mismatch fell inside a drill window you
logged first. A missed run, an unexplained mismatch, or an empty account sets the count back to 0;
the session the record started in partway is skipped. If a run's line can't be written, the run
exits 1 and pings `reconcile/fail` even with nothing to halt.

The rest of `desk-report` (step 3e): positions and open orders, STOP and its reason, reconcile
halts this month, the trial count and `data/trials.jsonl`'s SHA-256, backups (none while D2 isn't
done), and sleeps that overlapped market hours over the last 14 days, from `pmset -g log` (each sleep entry until the next wake, so a Mac asleep at the open counts; a dark wake that goes back to sleep counts again). The backup line says "none" until D2 is set up; it reads nothing yet. A part that can't be
read prints `UNREADABLE` and the report exits 1; the other parts still print.

Before every drill, log its window, which starts now: `target/release/bmv-trader drill-start --check
<halt|mismatch|stop-check|dead-man|stop> --minutes N` (at most 120). It appends to
`data/reconcile/drill_windows.jsonl`. A window can't be logged after the drill has started. Windows
may cover at most 120 minutes of a session, or the session doesn't count, and runs inside a window
must still hold the drill position, except during the STOP drill, which closes it. Any mismatch in
a window is excused, whatever its check; a drill's leftovers after the window ends reset the count.

Halt drill (Level A item 4), with the drill position and its stop open: run `drill-start --check halt --minutes 30`
first, then place a 1-share limit buy far below the market in the Alpaca paper page, then
`launchctl kickstart -k gui/$(id -u)/com.bmv-trader.reconcile`. Pass: the log says `halt in` at
most 5000 ms, the limit buy is gone and the drill stop is still at Alpaca. Then `rm data/STOP`.

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
  process list. It then applies the key's policy (only `bmv-free`, $0 a day, 30 requests a minute)
  before a test call.

- If a step fails, the error and the exact manual fix are kept in `data/llm_rotate_error.json`, and
  health shows FAIL until a rotation succeeds.
- **On the first manual `llm-rotate`, watch the output.** The regenerate output format and the policy
  flags are checked for real only then.
- Notes written while Tier 2 is down are saved as `*.facts.md`, and a later run replaces them with the
  worded note.

### Models and keys (DECISION S41)

- **Which model each job uses** is set in `deploy/models.json`: flip a line's `claude` between `true`
  (its Claude model) and `false` (its free OmniRoute route `bmv-free`).
  - **Engine jobs** (`engine`) apply on their next run.
  - **Desk agents** (`agents`) apply after `sudo scripts/clip-agents-setup.sh`.
  - **The signers and the notes are locked to Claude.**
- **Before the first run, set up `bmv-free` in the OmniRoute dashboard:**
  1. add free cloud providers (for example the Gemini free tier, Groq, OpenRouter's free models)
  2. make a combo named `bmv-free` with them in fallback order, and no Ollama
  3. allow `bmv-free` on the desk agents' OmniRoute key
  - **Until `bmv-free` exists, the daily rotation's test call fails.**
- **Keys (S43):** `scripts/keys.sh` shows every key, its renewal date, due date and state. `scripts/keys.sh renew` walks through the due ones: what each is, where to get a new one, a hidden paste, a test, then saving it and updating everything that uses it. A notification at 09:00 says when any key is 10 days or less from due, and one on the 1st reminds you to renew the desk budgets:

```sh
cp deploy/com.bmv-trader.keys.plist deploy/com.bmv-trader.monthly.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.bmv-trader.keys.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.bmv-trader.monthly.plist
```

- **Ollama is only for local replay trials:** `brew services start ollama` before one and
  `brew services stop ollama` after. Nothing else needs it.
- **Free cloud models' keys (S42):**
  - **OpenRouter:** make a management key at openrouter.ai/settings/management-keys, with an expiry such as 90 days. Store it with `security add-generic-password -s bmv-trader -a OPENROUTER_MANAGEMENT_KEY -w`. Add an OpenRouter connection named `openrouter-free` in OmniRoute, then run `target/release/bmv-trader free-keys` once and install the job:

```sh
cp deploy/com.bmv-trader.freekeys.plist ~/Library/LaunchAgents/
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.bmv-trader.freekeys.plist
```

  - **Others** (Groq, Google AI Studio): add each to `free_keys.providers` in `deploy/models.json` with `"auto": false` and `expires_days`, then run `scripts/free-key-add.sh <provider>`. The health check says when to replace it.
  - **Put every connection in the `bmv-free` combo** in OmniRoute's dashboard.
- **Agent keys files written before S41** hold only one model key. Give one its OmniRoute key, so
  its switch can flip to free, with `scripts/clip-key-swap.sh <agent> omniroute`, which keeps its
  desk token. Signers never get one.

### Quant Desk console page (DECISION S47)

The console's Desk page reads the desk as its own read-only login, `console-1`, role `console`. The desk refuses it every write. One-time setup, after the desk is rebuilt and restarted with S47:

```sh
/usr/local/libexec/bmv-desk/desk request '{"kind": "add_agent", "id": "console-1", "role": "console"}'
/usr/local/libexec/bmv-desk/desk request '{"kind": "issue_token", "agent": "console-1"}'
/usr/local/libexec/bmv-desk/desk pending
```

Approve both with `desk sign <id>` (add the agent first). Then store the token at the hidden prompt:

```sh
security add-generic-password -s bmv-desk -a console-1 -w
```

Restart `bmv-trader ui` and open Desk. It reads the desk every 2 minutes; until the token is there, it says so.

### Quant Desk evidence (DECISION S46)

The engine posts code-computed evidence to the desk as its own identity, `engine-1`, role `engine`. One-time setup:

```sh
/usr/local/libexec/bmv-desk/desk request '{"kind": "add_agent", "id": "engine-1", "role": "engine"}'
/usr/local/libexec/bmv-desk/desk request '{"kind": "issue_token", "agent": "engine-1"}'
/usr/local/libexec/bmv-desk/desk pending
```

Approve both with `desk sign <id>` (add the agent first). Then store the token at the hidden prompt:

```sh
security add-generic-password -s bmv-desk -a engine-1 -w
```

Post a trial's stress result to a strategy with `target/release/bmv-trader desk-evidence --strategy N --trial T`. The trial needs a recent `bmv-trader stress-battery` run, which records the deflated Sharpe.

### The drawdown peak in the desk (R13 step 3d)

Every paper session reads the drawdown peak from the desk as `engine-1` and raises it at start, so the
desk must be up and must have a peak; deleting `data/equity_peak.json` changes nothing. Once, after the
desk binary with the `equity_peaks` table is installed:

```sh
CARGO_TARGET_DIR=target/desk cargo build --release -p desk   # as you
scripts/build-desk-sign.sh target/desk/release               # as you
sudo scripts/desk-setup.sh target/desk/release               # installs the binaries, restarts com.bmv.desk
cargo build --release
target/release/bmv-trader peak-seed                          # today's paper equity becomes the first peak
```

`peak-seed` refuses if the desk already has a peak. It seeds the higher of today's equity and the old
`data/equity_peak.json`, if that exists. After that only a session raises it, and no request or
agent can lower it. There is no reset: a wrong seed, or a withdrawal that leaves equity for good
below the peak, keeps every session halted until a reset path is built (an owner-signed one is
the follow-up).

