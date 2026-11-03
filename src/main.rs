use bmv_trader::{broker, config, config::Mode};
use std::process::exit;

fn main() {
    let cfg = config::load().unwrap_or_else(|e| fail(&e));

    match cfg.mode() {
        Mode::Paper => eprintln!("mode: PAPER"),
        Mode::Live => eprintln!("\x1b[1;41m  LIVE TRADING — REAL MONEY  \x1b[0m"),
    }

    let alpaca = broker::Alpaca::new(cfg).unwrap_or_else(|e| fail(&e));
    match std::env::args().nth(1).as_deref() {
        Some("account") => {
            let a = alpaca.account().unwrap_or_else(|e| fail(&e));
            for (k, v) in [
                ("status", a.status),
                ("currency", a.currency),
                ("cash", a.cash.to_string()),
                ("buying_power", a.buying_power.to_string()),
                ("equity", a.equity.to_string()),
                ("today_pnl", (a.equity - a.last_equity).to_string()),
                ("trading_blocked", a.trading_blocked.to_string()),
                ("account_blocked", a.account_blocked.to_string()),
            ] {
                println!("{k:>16}: {v}");
            }
        }
        _ => fail("usage: bmv-trader account"),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    exit(1)
}
