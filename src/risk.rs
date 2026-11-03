//! The risk gate (DESIGN §9): the only way to turn a Candidate into an OrderIntent.
//! Pure functions over a snapshot, so the backtest and the live engine apply identical rules.
//! Sign convention: limits are positive magnitudes; a loss is a negative `Usd`.
//!
//! The engine owns, and folds into the snapshot: the Claude call cap and error streak (no vetting →
//! no candidate), account `trading_blocked` (→ `halted`), and a kill-switch read error (→ `kill_switch`).

use crate::config::Mode;
use crate::money::{Price, Qty, Usd};
use crate::strategy::Candidate;
use crate::vetting::Verdict;
use std::fmt;

/// Hard limits for one mode. Fields are private, so `for_mode` is the only way to get one, and the
/// mode travels into every OrderIntent so the broker can refuse paper limits on a live account.
#[derive(Debug, PartialEq, Eq)]
pub struct Limits {
    mode: Mode,
    max_notional: Usd,
    max_risk_per_trade: Usd,
    max_positions: usize,
    daily_loss_halt: Usd,
    max_trades_per_day: usize,
    consecutive_loss_halt: u32,
    max_per_cluster: usize,
    /// Minimum R = max(entry × min_r_bps / 10 000, min_r).
    min_r_bps: i64,
    min_r: Usd,
}

impl Limits {
    pub fn for_mode(mode: Mode) -> Self {
        let d = |dollars: i64| Usd::from_cents(dollars * 100);
        let (notional, risk, positions, halt, trades, losses, cluster) = match mode {
            Mode::Paper => (2_500, 250, 5, 500, 10, 3, 2),
            Mode::Live => (500, 50, 2, 100, 3, 2, 1),
        };
        Limits {
            mode,
            max_notional: d(notional),
            max_risk_per_trade: d(risk),
            max_positions: positions,
            daily_loss_halt: d(halt),
            max_trades_per_day: trades,
            consecutive_loss_halt: losses,
            max_per_cluster: cluster,
            min_r_bps: 15,
            min_r: Usd::from_cents(5),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
    /// What a pending candidate reserves while Claude decides (its size isn't known yet).
    pub fn max_risk_per_trade(&self) -> Usd {
        self.max_risk_per_trade
    }
    pub fn daily_loss_halt(&self) -> Usd {
        self.daily_loss_halt
    }
    pub fn consecutive_loss_halt(&self) -> u32 {
        self.consecutive_loss_halt
    }

    fn min_r_for(&self, entry: Price) -> Usd {
        let bps = entry
            .cents()
            .saturating_mul(self.min_r_bps)
            .saturating_add(9_999)
            / 10_000;
        Usd::from_cents(bps).max(self.min_r)
    }
}

/// Correlated names count against one bucket (DESIGN §9). Expects an uppercase symbol.
// shortcut: hard-coded for the v1 watchlist; unknown symbols are their own cluster. Replace with
// sector data when the research universe (DESIGN §13) goes beyond these names.
pub fn cluster_of(symbol: &str) -> &str {
    match symbol {
        "SPY" | "QQQ" => "index",
        "NVDA" | "AMD" => "semis",
        "AAPL" | "MSFT" | "META" | "AMZN" | "GOOGL" => "megacap-tech",
        other => other,
    }
}

/// Something already using risk budget: an open position, a working entry, or a reservation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exposure {
    symbol: String,
    risk: Usd,
}

impl Exposure {
    /// `risk` is dollars lost if the stop fills. Below zero (stop moved above entry) counts as zero,
    /// so an exposure can never add budget.
    pub fn new(symbol: &str, risk: Usd) -> Self {
        Exposure {
            symbol: symbol.to_ascii_uppercase(),
            risk: risk.max(Usd::ZERO),
        }
    }
    pub fn symbol(&self) -> &str {
        &self.symbol
    }
    pub fn risk(&self) -> Usd {
        self.risk
    }
}

/// Everything the gate needs to know about now. Defaults fail closed: no entries, no buying power.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RiskState {
    pub kill_switch: bool,
    pub halted: bool,
    pub entries_allowed: bool,
    pub pnl_today: Usd,
    pub buying_power: Usd,
    pub consecutive_losses: u32,
    /// Symbols with an order or reservation today; one trade per symbol, so its length is the trade count.
    pub traded_today: Vec<String>,
    /// Positions, working entries and reservations, excluding this candidate's own reservation.
    pub open: Vec<Exposure>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    KillSwitch,
    Halted,
    OutsideWindow,
    DailyLoss,
    ConsecutiveLosses,
    MaxTrades,
    SymbolTradedToday,
    MaxPositions,
    Cluster,
    ClaudeRejected,
    MinR,
    ZeroQty,
    OpenRisk,
    BuyingPower,
    /// A post-sizing check failed: a bug in this module, never a market condition.
    Invariant,
}

impl fmt::Display for Rejection {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// A sized, risk-checked bracket order. Fields are private and it isn't Clone: only `gate`
/// creates one, and the broker's submit takes it by value, so it can't be sent twice.
#[derive(Debug, PartialEq, Eq)]
pub struct OrderIntent {
    mode: Mode,
    symbol: String,
    qty: Qty,
    entry: Price,
    stop: Price,
    target: Price,
}

impl OrderIntent {
    /// The mode whose limits approved this order.
    pub fn mode(&self) -> Mode {
        self.mode
    }
    pub fn symbol(&self) -> &str {
        &self.symbol
    }
    pub fn qty(&self) -> Qty {
        self.qty
    }
    pub fn entry(&self) -> Price {
        self.entry
    }
    pub fn stop(&self) -> Price {
        self.stop
    }
    pub fn target(&self) -> Price {
        self.target
    }
    pub fn risk(&self) -> Usd {
        (self.entry - self.stop) * self.qty
    }
}

/// Applies every §9 rule, cheapest first. Size = min(risk cap / R, notional cap / entry), then
/// Claude's reduction, then shrunk to fit the remaining daily risk budget and the buying power.
pub fn gate(
    c: &Candidate,
    v: Verdict,
    s: &RiskState,
    l: &Limits,
) -> Result<OrderIntent, Rejection> {
    use Rejection::*;
    let symbol = c.symbol();
    let cluster = cluster_of(symbol);
    let checks = [
        (s.kill_switch, KillSwitch),
        (s.halted, Halted),
        (!s.entries_allowed, OutsideWindow),
        (s.pnl_today <= Usd::ZERO - l.daily_loss_halt, DailyLoss),
        (
            s.consecutive_losses >= l.consecutive_loss_halt,
            ConsecutiveLosses,
        ),
        (s.traded_today.len() >= l.max_trades_per_day, MaxTrades),
        (
            s.traded_today
                .iter()
                .any(|t| t.eq_ignore_ascii_case(symbol)),
            SymbolTradedToday,
        ),
        (s.open.len() >= l.max_positions, MaxPositions),
        (
            s.open
                .iter()
                .filter(|e| cluster_of(&e.symbol) == cluster)
                .count()
                >= l.max_per_cluster,
            Cluster,
        ),
        (v == Verdict::Reject, ClaudeRejected),
        (c.r() < l.min_r_for(c.entry()), MinR),
    ];
    if let Some((_, why)) = checks.iter().find(|(failed, _)| *failed) {
        return Err(*why);
    }

    let r = c.r();
    let entry = c.entry().as_usd();
    let by_risk = l.max_risk_per_trade.units_of(r).ok_or(ZeroQty)?;
    let by_notional = l.max_notional.units_of(entry).ok_or(ZeroQty)?;
    let mut qty = by_risk.min(by_notional).get();
    if let Verdict::Reduce(f) = v {
        qty = u32::try_from(u64::from(qty) * u64::from(f.get()) / 100).map_err(|_| Invariant)?;
    }
    let qty = Qty::new(qty).ok_or(ZeroQty)?;

    // Total open risk may not exceed what's left of today's loss budget; profits don't extend it.
    let used = s.open.iter().fold(Usd::ZERO, |acc, e| acc + e.risk);
    let budget = l.daily_loss_halt + s.pnl_today.min(Usd::ZERO);
    let qty = qty.min((budget - used).units_of(r).ok_or(OpenRisk)?);
    let qty = qty.min(s.buying_power.units_of(entry).ok_or(BuyingPower)?);

    let o = OrderIntent {
        mode: l.mode,
        symbol: symbol.to_string(),
        qty,
        entry: c.entry(),
        stop: c.stop(),
        target: c.target(),
    };
    let notional = o.entry * o.qty;
    let within = notional <= l.max_notional
        && o.risk() <= l.max_risk_per_trade
        && used + o.risk() <= budget
        && notional <= s.buying_power;
    if within { Ok(o) } else { Err(Invariant) }
}

/// Fixed cases run at startup: every rejection reason must fire, and a normal trade must pass with
/// the exact expected size. A gate that blocks everything is as broken as one that blocks nothing.
pub fn self_test(l: &Limits) -> Result<(), String> {
    use Rejection::*;
    let p = |c| Price::from_cents(c).ok_or("self-test price");
    let spy = Candidate::long("SPY", p(10_000)?, p(9_900)?, p(10_200)?, String::new())?;
    let tight = Candidate::long("SPY", p(10_000)?, p(9_999)?, p(10_002)?, String::new())?;
    let ok = RiskState {
        entries_allowed: true,
        buying_power: Usd::from_cents(1_000_000_000),
        ..RiskState::default()
    };
    let syms = |prefix: &str, n: usize| (0..n).map(|i| format!("{prefix}{i}")).collect::<Vec<_>>();
    let exposures =
        |names: Vec<String>, risk: Usd| names.iter().map(|n| Exposure::new(n, risk)).collect();
    let cent = Usd::from_cents(1);

    // $1 R on a $100 stock: min(risk cap / $1, notional cap / $100)
    let want = (l.max_risk_per_trade.cents() / 100).min(l.max_notional.cents() / 10_000);
    match gate(&spy, Verdict::Approve, &ok, l) {
        Ok(o) if i64::from(o.qty().get()) == want && o.mode() == l.mode => {}
        other => {
            return Err(format!(
                "risk self-test 'normal trade': expected {want} shares, got {other:?}"
            ));
        }
    }

    let reduce_1pct =
        Verdict::Reduce(crate::vetting::Factor::percent(1).ok_or("self-test factor")?);
    let cases: [(&str, &Candidate, Verdict, RiskState, Rejection); 14] = [
        (
            "kill switch",
            &spy,
            Verdict::Approve,
            RiskState {
                kill_switch: true,
                ..ok.clone()
            },
            KillSwitch,
        ),
        (
            "halted",
            &spy,
            Verdict::Approve,
            RiskState {
                halted: true,
                ..ok.clone()
            },
            Halted,
        ),
        (
            "outside window",
            &spy,
            Verdict::Approve,
            RiskState {
                entries_allowed: false,
                ..ok.clone()
            },
            OutsideWindow,
        ),
        (
            "daily loss",
            &spy,
            Verdict::Approve,
            RiskState {
                pnl_today: Usd::ZERO - l.daily_loss_halt,
                ..ok.clone()
            },
            DailyLoss,
        ),
        (
            "consecutive losses",
            &spy,
            Verdict::Approve,
            RiskState {
                consecutive_losses: l.consecutive_loss_halt,
                ..ok.clone()
            },
            ConsecutiveLosses,
        ),
        (
            "max trades",
            &spy,
            Verdict::Approve,
            RiskState {
                traded_today: syms("T", l.max_trades_per_day),
                ..ok.clone()
            },
            MaxTrades,
        ),
        (
            "symbol traded",
            &spy,
            Verdict::Approve,
            RiskState {
                traded_today: vec!["spy".into()],
                ..ok.clone()
            },
            SymbolTradedToday,
        ),
        (
            "max positions",
            &spy,
            Verdict::Approve,
            RiskState {
                open: exposures(syms("P", l.max_positions), cent),
                ..ok.clone()
            },
            MaxPositions,
        ),
        (
            "cluster",
            &spy,
            Verdict::Approve,
            RiskState {
                open: exposures(vec!["QQQ".into(); l.max_per_cluster], cent),
                ..ok.clone()
            },
            Cluster,
        ),
        (
            "claude reject",
            &spy,
            Verdict::Reject,
            ok.clone(),
            ClaudeRejected,
        ),
        ("min R", &tight, Verdict::Approve, ok.clone(), MinR),
        ("zero qty", &spy, reduce_1pct, ok.clone(), ZeroQty),
        (
            "open risk",
            &spy,
            Verdict::Approve,
            RiskState {
                open: exposures(vec!["X".into()], l.daily_loss_halt),
                ..ok.clone()
            },
            OpenRisk,
        ),
        (
            "buying power",
            &spy,
            Verdict::Approve,
            RiskState {
                buying_power: Usd::ZERO,
                ..ok.clone()
            },
            BuyingPower,
        ),
    ];
    if Exposure::new("Y", Usd::ZERO - l.daily_loss_halt).risk() != Usd::ZERO {
        return Err("risk self-test 'negative exposure': must count as zero risk".into());
    }
    for (label, c, v, s, want) in cases {
        let got = gate(c, v, &s, l).err();
        if got != Some(want) {
            return Err(format!(
                "risk self-test '{label}': expected {want:?}, got {got:?}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vetting::Factor;
    use proptest::prelude::*;

    fn p(c: i64) -> Price {
        Price::from_cents(c).unwrap()
    }
    fn usd(dollars: i64) -> Usd {
        Usd::from_cents(dollars * 100)
    }
    fn cand(symbol: &str, entry: i64, stop: i64) -> Candidate {
        Candidate::long(
            symbol,
            p(entry),
            p(stop),
            p(entry + 2 * (entry - stop)),
            String::new(),
        )
        .unwrap()
    }
    fn open_state() -> RiskState {
        RiskState {
            entries_allowed: true,
            buying_power: usd(1_000_000),
            ..RiskState::default()
        }
    }
    fn exposure(symbol: &str, risk_dollars: i64) -> Exposure {
        Exposure::new(symbol, usd(risk_dollars))
    }
    fn paper() -> Limits {
        Limits::for_mode(Mode::Paper)
    }
    fn live() -> Limits {
        Limits::for_mode(Mode::Live)
    }
    fn qty(r: &Result<OrderIntent, Rejection>) -> u32 {
        r.as_ref().map(|o| o.qty().get()).unwrap_or(0)
    }

    #[test]
    fn limits_match_the_design_table() {
        let row = |l: &Limits| {
            (
                l.mode,
                l.max_notional,
                l.max_risk_per_trade,
                l.max_positions,
                l.daily_loss_halt,
                l.max_trades_per_day,
                l.consecutive_loss_halt,
                l.max_per_cluster,
                l.min_r_bps,
                l.min_r,
            )
        };
        assert_eq!(
            row(&paper()),
            (
                Mode::Paper,
                usd(2_500),
                usd(250),
                5,
                usd(500),
                10,
                3,
                2,
                15,
                Usd::from_cents(5)
            )
        );
        assert_eq!(
            row(&live()),
            (
                Mode::Live,
                usd(500),
                usd(50),
                2,
                usd(100),
                3,
                2,
                1,
                15,
                Usd::from_cents(5)
            )
        );
    }

    #[test]
    fn self_test_passes_for_both_modes() {
        assert_eq!(self_test(&paper()), Ok(()));
        assert_eq!(self_test(&live()), Ok(()));
    }

    // --- sizing -------------------------------------------------------------------------------

    #[test]
    fn size_is_the_smaller_of_the_two_caps() {
        // $50 stock, $1 R: risk cap 250 shares, notional cap 50 → 50
        let o = gate(
            &cand("XYZ", 5_000, 4_900),
            Verdict::Approve,
            &open_state(),
            &paper(),
        )
        .unwrap();
        assert_eq!(o.qty().get(), 50);
        // $20 stock, $2 R: both caps exactly hit at 125
        let o = gate(
            &cand("XYZ", 2_000, 1_800),
            Verdict::Approve,
            &open_state(),
            &paper(),
        )
        .unwrap();
        assert_eq!(
            (o.qty().get(), o.risk(), o.entry() * o.qty()),
            (125, usd(250), usd(2_500))
        );
        // $20 stock, $5 R: risk cap 50 < notional 125 → 50
        let o = gate(
            &cand("XYZ", 2_000, 1_500),
            Verdict::Approve,
            &open_state(),
            &paper(),
        )
        .unwrap();
        assert_eq!(o.qty().get(), 50);
    }

    #[test]
    fn a_stock_above_the_notional_cap_cannot_be_bought() {
        let r = gate(
            &cand("XYZ", 300_000, 299_000),
            Verdict::Approve,
            &open_state(),
            &paper(),
        );
        assert_eq!(r, Err(Rejection::ZeroQty));
    }

    #[test]
    fn claude_can_only_shrink() {
        let c = cand("XYZ", 5_000, 4_900);
        let full = qty(&gate(&c, Verdict::Approve, &open_state(), &paper()));
        let half = qty(&gate(
            &c,
            Verdict::Reduce(Factor::percent(50).unwrap()),
            &open_state(),
            &paper(),
        ));
        assert_eq!((full, half), (50, 25));
        let tiny = gate(
            &c,
            Verdict::Reduce(Factor::percent(1).unwrap()),
            &open_state(),
            &paper(),
        );
        assert_eq!(tiny, Err(Rejection::ZeroQty)); // 1% of 50 rounds to 0 shares
        assert_eq!(
            gate(&c, Verdict::Reject, &open_state(), &paper()),
            Err(Rejection::ClaudeRejected)
        );
    }

    #[test]
    fn order_keeps_the_candidates_prices_and_carries_the_mode() {
        let c = cand("NVDA", 18_740, 18_610);
        let o = gate(&c, Verdict::Approve, &open_state(), &live()).unwrap();
        assert_eq!(
            (o.symbol(), o.entry(), o.stop(), o.target()),
            ("NVDA", c.entry(), c.stop(), c.target())
        );
        assert_eq!(o.mode(), Mode::Live);
    }

    #[test]
    fn minimum_r_is_enforced_here_too() {
        // $100 stock: 0.15% = 15¢. 14¢ fails, 15¢ passes; $10 stock: the 5¢ floor decides.
        assert_eq!(
            gate(
                &cand("XYZ", 10_000, 9_986),
                Verdict::Approve,
                &open_state(),
                &paper()
            )
            .err(),
            Some(Rejection::MinR)
        );
        assert!(
            gate(
                &cand("XYZ", 10_000, 9_985),
                Verdict::Approve,
                &open_state(),
                &paper()
            )
            .is_ok()
        );
        assert_eq!(
            gate(
                &cand("XYZ", 1_000, 996),
                Verdict::Approve,
                &open_state(),
                &paper()
            )
            .err(),
            Some(Rejection::MinR)
        );
        assert!(
            gate(
                &cand("XYZ", 1_000, 995),
                Verdict::Approve,
                &open_state(),
                &paper()
            )
            .is_ok()
        );
    }

    #[test]
    fn buying_power_caps_the_size() {
        let c = cand("XYZ", 2_000, 1_800); // caps allow 125 shares ($2,500)
        let s = RiskState {
            buying_power: usd(1_000),
            ..open_state()
        };
        assert_eq!(qty(&gate(&c, Verdict::Approve, &s, &paper())), 50);
        let s = RiskState {
            buying_power: usd(19),
            ..open_state()
        };
        assert_eq!(
            gate(&c, Verdict::Approve, &s, &paper()).err(),
            Some(Rejection::BuyingPower)
        );
    }

    // --- every §9 rule: one passing and one breached case --------------------------------------

    #[test]
    fn kill_switch_halt_and_window() {
        let c = cand("SPY", 10_000, 9_900);
        assert!(gate(&c, Verdict::Approve, &open_state(), &paper()).is_ok());
        let breach = |f: fn(&mut RiskState)| {
            let mut s = open_state();
            f(&mut s);
            gate(&c, Verdict::Approve, &s, &paper()).err()
        };
        assert_eq!(
            breach(|s| s.kill_switch = true),
            Some(Rejection::KillSwitch)
        );
        assert_eq!(breach(|s| s.halted = true), Some(Rejection::Halted));
        assert_eq!(
            breach(|s| s.entries_allowed = false),
            Some(Rejection::OutsideWindow)
        );
    }

    #[test]
    fn daily_loss_halt() {
        let c = cand("SPY", 10_000, 9_900);
        let at = |pnl: i64| {
            gate(
                &c,
                Verdict::Approve,
                &RiskState {
                    pnl_today: Usd::from_cents(pnl),
                    ..open_state()
                },
                &paper(),
            )
        };
        // −$400: $100 of budget left, $1 R → still tradable (25 shares, notional-capped)
        assert_eq!(at(-40_000).map(|o| o.qty().get()), Ok(25));
        // −$499.99: not halted yet, but 1¢ of budget can't cover even one share's risk
        assert_eq!(at(-49_999).err(), Some(Rejection::OpenRisk));
        assert_eq!(at(-50_000).err(), Some(Rejection::DailyLoss));
        assert_eq!(at(-90_000).err(), Some(Rejection::DailyLoss));
    }

    #[test]
    fn consecutive_losses_and_trade_count() {
        let c = cand("SPY", 10_000, 9_900);
        let st = |losses, trades: usize| RiskState {
            consecutive_losses: losses,
            traded_today: (0..trades).map(|i| format!("T{i}")).collect(),
            ..open_state()
        };
        assert!(gate(&c, Verdict::Approve, &st(2, 9), &paper()).is_ok());
        assert_eq!(
            gate(&c, Verdict::Approve, &st(3, 0), &paper()).err(),
            Some(Rejection::ConsecutiveLosses)
        );
        assert_eq!(
            gate(&c, Verdict::Approve, &st(0, 10), &paper()).err(),
            Some(Rejection::MaxTrades)
        );
        assert!(gate(&c, Verdict::Approve, &st(1, 2), &live()).is_ok());
        assert_eq!(
            gate(&c, Verdict::Approve, &st(2, 0), &live()).err(),
            Some(Rejection::ConsecutiveLosses)
        );
        assert_eq!(
            gate(&c, Verdict::Approve, &st(0, 3), &live()).err(),
            Some(Rejection::MaxTrades)
        );
    }

    #[test]
    fn one_trade_per_symbol_per_day_ignoring_case() {
        let c = cand("SPY", 10_000, 9_900);
        let s = RiskState {
            traded_today: vec!["AAPL".into()],
            ..open_state()
        };
        assert!(gate(&c, Verdict::Approve, &s, &paper()).is_ok());
        let s = RiskState {
            traded_today: vec!["spy".into()],
            ..open_state()
        };
        assert_eq!(
            gate(&c, Verdict::Approve, &s, &paper()).err(),
            Some(Rejection::SymbolTradedToday)
        );
    }

    #[test]
    fn max_open_exposures() {
        let c = cand("TSLA", 10_000, 9_900);
        let with = |n: usize| RiskState {
            open: (0..n).map(|i| exposure(&format!("S{i}"), 10)).collect(),
            ..open_state()
        };
        assert!(gate(&c, Verdict::Approve, &with(4), &paper()).is_ok());
        assert_eq!(
            gate(&c, Verdict::Approve, &with(5), &paper()).err(),
            Some(Rejection::MaxPositions)
        );
        assert!(gate(&c, Verdict::Approve, &with(1), &live()).is_ok());
        assert_eq!(
            gate(&c, Verdict::Approve, &with(2), &live()).err(),
            Some(Rejection::MaxPositions)
        );
    }

    #[test]
    fn correlated_names_share_a_cluster_limit() {
        let c = cand("QQQ", 10_000, 9_900);
        let with = |names: &[&str]| RiskState {
            open: names.iter().map(|n| exposure(n, 10)).collect(),
            ..open_state()
        };
        assert!(gate(&c, Verdict::Approve, &with(&["SPY"]), &paper()).is_ok());
        assert!(
            gate(&c, Verdict::Approve, &with(&["SPY2", "QQQX"]), &paper()).is_ok(),
            "unknown names are their own cluster"
        );
        assert_eq!(
            gate(&c, Verdict::Approve, &with(&["SPY", "qqq"]), &paper()).err(),
            Some(Rejection::Cluster)
        );
        assert_eq!(
            gate(&c, Verdict::Approve, &with(&["SPY"]), &live()).err(),
            Some(Rejection::Cluster)
        );
    }

    #[test]
    fn open_risk_never_exceeds_the_remaining_daily_budget() {
        let c = cand("XYZ", 2_000, 1_800); // $2 R → 125 shares, $250 risk
        // $400 already at risk, $500 budget → $100 left → 50 shares
        let s = RiskState {
            open: vec![exposure("A", 200), exposure("B", 200)],
            ..open_state()
        };
        let o = gate(&c, Verdict::Approve, &s, &paper()).unwrap();
        assert_eq!((o.qty().get(), o.risk()), (50, usd(100)));
        // a $300 loss today leaves $200; $150 used → $50 left → 25 shares
        let s = RiskState {
            pnl_today: usd(-300),
            open: vec![exposure("A", 150)],
            ..open_state()
        };
        assert_eq!(qty(&gate(&c, Verdict::Approve, &s, &paper())), 25);
        // profits don't extend the budget
        let s = RiskState {
            pnl_today: usd(5_000),
            open: vec![exposure("A", 400)],
            ..open_state()
        };
        assert_eq!(qty(&gate(&c, Verdict::Approve, &s, &paper())), 50);
        // nothing left
        let s = RiskState {
            open: vec![exposure("A", 499)],
            ..open_state()
        };
        assert_eq!(
            gate(&c, Verdict::Approve, &s, &paper()).err(),
            Some(Rejection::OpenRisk)
        );
    }

    #[test]
    fn a_negative_exposure_never_adds_budget() {
        assert_eq!(exposure("A", -300).risk(), Usd::ZERO);
        let c = cand("XYZ", 2_000, 1_800);
        let s = RiskState {
            open: vec![exposure("A", 499), exposure("B", -300)],
            ..open_state()
        };
        assert_eq!(
            gate(&c, Verdict::Approve, &s, &paper()).err(),
            Some(Rejection::OpenRisk)
        );
    }

    // --- properties: hold for any input -------------------------------------------------------

    const POOL: [&str; 8] = ["SPY", "QQQ", "NVDA", "AMD", "AAPL", "MSFT", "TSLA", "XYZ"];

    fn verdict() -> impl Strategy<Value = Verdict> {
        prop_oneof![
            Just(Verdict::Approve),
            Just(Verdict::Reject),
            (1u8..=99).prop_map(|p| Verdict::Reduce(Factor::percent(p).unwrap())),
        ]
    }

    fn state() -> impl Strategy<Value = RiskState> {
        (
            (any::<bool>(), any::<bool>(), any::<bool>()),
            -80_000i64..80_000,
            0i64..10_000_000,
            0u32..5,
            proptest::collection::vec(proptest::sample::select(POOL.to_vec()), 0..12),
            proptest::collection::vec(
                (proptest::sample::select(POOL.to_vec()), -10_000i64..80_000),
                0..7,
            ),
        )
            .prop_map(|((kill, halted, entries), pnl, bp, losses, traded, open)| {
                RiskState {
                    kill_switch: kill,
                    halted,
                    entries_allowed: entries,
                    pnl_today: Usd::from_cents(pnl),
                    buying_power: Usd::from_cents(bp),
                    consecutive_losses: losses,
                    traded_today: traded.into_iter().map(String::from).collect(),
                    open: open
                        .into_iter()
                        .map(|(s, r)| Exposure::new(s, Usd::from_cents(r)))
                        .collect(),
                }
            })
    }

    /// Prices from 1¢ to $100k, stop anywhere from 1¢ below entry down to 1¢.
    fn candidate() -> impl Strategy<Value = Candidate> {
        (
            proptest::sample::select(POOL.to_vec()),
            2i64..10_000_000,
            1u32..10_000,
        )
            .prop_map(|(sym, entry, frac)| {
                let r = (entry * i64::from(frac) / 10_000).clamp(1, entry - 1);
                cand(sym, entry, entry - r)
            })
    }

    fn breaches_a_precondition(c: &Candidate, v: Verdict, s: &RiskState, l: &Limits) -> bool {
        let cluster = cluster_of(c.symbol());
        s.kill_switch
            || s.halted
            || !s.entries_allowed
            || s.pnl_today <= Usd::ZERO - l.daily_loss_halt
            || s.consecutive_losses >= l.consecutive_loss_halt
            || s.traded_today.len() >= l.max_trades_per_day
            || s.traded_today.iter().any(|t| t == c.symbol())
            || s.open.len() >= l.max_positions
            || s.open
                .iter()
                .filter(|e| cluster_of(e.symbol()) == cluster)
                .count()
                >= l.max_per_cluster
            || v == Verdict::Reject
            || c.r() < l.min_r_for(c.entry())
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2048))]

        #[test]
        fn approved_orders_never_break_a_limit_and_breaches_always_reject(
            c in candidate(), v in verdict(), s in state(), is_live in any::<bool>(),
        ) {
            let l = if is_live { live() } else { paper() };
            match gate(&c, v, &s, &l) {
                Ok(o) => {
                    prop_assert!(!breaches_a_precondition(&c, v, &s, &l));
                    let used = s.open.iter().fold(Usd::ZERO, |a, e| a + e.risk());
                    prop_assert!(o.entry() * o.qty() <= l.max_notional);
                    prop_assert!(o.entry() * o.qty() <= s.buying_power);
                    prop_assert!(o.risk() <= l.max_risk_per_trade);
                    prop_assert!(used + o.risk() <= l.daily_loss_halt + s.pnl_today.min(Usd::ZERO));
                    prop_assert_eq!((o.symbol(), o.entry(), o.stop(), o.target()), (c.symbol(), c.entry(), c.stop(), c.target()));
                    prop_assert_eq!(o.mode(), l.mode());
                }
                Err(why) => prop_assert_ne!(why, Rejection::Invariant),
            }
            if breaches_a_precondition(&c, v, &s, &l) {
                prop_assert!(gate(&c, v, &s, &l).is_err());
            }
        }

        #[test]
        fn a_clean_state_sizes_exactly(c in candidate(), pct in proptest::option::of(1u8..=99), is_live in any::<bool>()) {
            let l = if is_live { live() } else { paper() };
            prop_assume!(c.r() >= l.min_r_for(c.entry()));
            let v = pct.map_or(Verdict::Approve, |p| Verdict::Reduce(Factor::percent(p).unwrap()));
            let (r, e) = (c.r().cents(), c.entry().cents());
            let caps = (l.max_risk_per_trade.cents() / r).min(l.max_notional.cents() / e);
            let reduced = pct.map_or(caps, |p| caps * i64::from(p) / 100);
            let want = reduced.min(l.daily_loss_halt.cents() / r); // the budget
            match gate(&c, v, &open_state(), &l) {
                Ok(o) => prop_assert_eq!(i64::from(o.qty().get()), want),
                Err(why) => {
                    prop_assert_eq!(want, 0, "rejected {:?} though {} shares fit", why, want);
                    prop_assert!(matches!(why, Rejection::ZeroQty | Rejection::OpenRisk));
                }
            }
        }
    }
}
