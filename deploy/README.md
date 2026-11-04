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
# one manual run first, to measure what an analysis costs (counts toward the budget)
mkdir -p data/desk
target/release/bmv-trader desk-analyse --symbols AAPL --date 2026-11-03
```

Install, check and remove work like the backstop above, with `com.bmv-trader.desk.plist`. The log is
`data/desk/run.log`; `bmv-trader desk-report` shows progress at any time.
