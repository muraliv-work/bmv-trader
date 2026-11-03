//! Money as integer cents: exact arithmetic, no float rounding in limits or sizing.
//! Arithmetic saturates instead of wrapping, so an absurd value can't flip sign. That only fails
//! *upper*-bound checks closed; risk code must not rely on it for lower bounds (equity ≥ x).

use std::fmt;
use std::num::NonZeroU32;
use std::ops::{Add, Mul, Sub};

/// Signed dollars in cents: P&L, equity, limits, per-share differences.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Usd(i64);

/// A price in cents, always > 0.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Price(i64);

/// Whole shares, never 0.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Qty(NonZeroU32);

impl Usd {
    pub const ZERO: Usd = Usd(0);

    pub fn from_cents(c: i64) -> Self {
        Usd(c)
    }

    pub fn cents(self) -> i64 {
        self.0
    }

    /// Parses Alpaca's decimal strings ("24950.1", "-12.345"). Beyond 2 decimals rounds half away from zero.
    pub fn parse(s: &str) -> Result<Self, String> {
        let bad = || format!("not a dollar amount: {s:?}");
        let (neg, digits) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let (int, frac) = match digits.split_once('.') {
            Some((_, "")) => return Err(bad()), // "5."
            Some(parts) => parts,
            None => (digits, ""),
        };
        let all_digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
        if int.is_empty() || !all_digits(int) || !all_digits(frac) {
            return Err(bad());
        }
        let fb = frac.as_bytes();
        let digit = |i: usize| fb.get(i).map_or(0, |b| i64::from(b - b'0'));
        let round_up = i64::from(digit(2) >= 5);
        let cents = int
            .parse::<i64>()
            .ok()
            .and_then(|d| d.checked_mul(100))
            .and_then(|c| c.checked_add(digit(0) * 10 + digit(1) + round_up))
            .ok_or_else(bad)?;
        Ok(Usd(if neg { -cents } else { cents }))
    }

    /// How many whole `per_unit` fit in `self` (floor). None — meaning "no trade" — when that's 0,
    /// when either side isn't positive, or when it doesn't fit a u32.
    pub fn units_of(self, per_unit: Usd) -> Option<Qty> {
        if self.0 <= 0 || per_unit.0 <= 0 {
            return None;
        }
        u32::try_from(self.0 / per_unit.0).ok().and_then(Qty::new)
    }
}

/// R for a long: entry − stop per share. None unless the stop is strictly below entry.
pub fn risk_per_share(entry: Price, stop: Price) -> Option<Usd> {
    let r = entry - stop;
    (r > Usd::ZERO).then_some(r)
}

impl Price {
    /// The lowest valid price, used as the worst case when a computed price isn't valid.
    pub const PENNY: Price = Price(1);

    pub fn from_cents(c: i64) -> Option<Self> {
        (c > 0).then_some(Price(c))
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        Price::from_cents(Usd::parse(s)?.0).ok_or_else(|| format!("price must be > 0: {s:?}"))
    }

    /// From a float bar price, rounded up to the cent (e.g. a buy limit). None if not finite or ≤ 0.
    pub fn from_f64_ceil(x: f64) -> Option<Self> {
        // the epsilon absorbs float noise like 187.33 * 100 = 18733.000000000004
        Self::from_f64(x, |c| (c - 1e-6).ceil())
    }

    /// From a float bar price, rounded down to the cent (e.g. a long stop). None if not finite or ≤ 0.
    pub fn from_f64_floor(x: f64) -> Option<Self> {
        Self::from_f64(x, |c| (c + 1e-6).floor())
    }

    fn from_f64(x: f64, round: impl Fn(f64) -> f64) -> Option<Self> {
        let c = round(x * 100.0);
        // the bound keeps the cast exact; no real price is near it
        (c.is_finite() && c > 0.0 && c < 1e15).then_some(Price(c as i64))
    }

    pub fn cents(self) -> i64 {
        self.0
    }

    pub fn as_usd(self) -> Usd {
        Usd(self.0)
    }
}

impl Qty {
    pub fn new(n: u32) -> Option<Self> {
        NonZeroU32::new(n).map(Qty)
    }

    pub fn get(self) -> u32 {
        self.0.get()
    }
}

impl Add for Usd {
    type Output = Usd;
    fn add(self, o: Usd) -> Usd {
        Usd(self.0.saturating_add(o.0))
    }
}

impl Sub for Usd {
    type Output = Usd;
    fn sub(self, o: Usd) -> Usd {
        Usd(self.0.saturating_sub(o.0))
    }
}

/// Per-share difference, e.g. R = entry − stop.
impl Sub for Price {
    type Output = Usd;
    fn sub(self, o: Price) -> Usd {
        Usd(self.0.saturating_sub(o.0))
    }
}

impl Mul<Qty> for Usd {
    type Output = Usd;
    fn mul(self, q: Qty) -> Usd {
        Usd(self.0.saturating_mul(i64::from(q.get())))
    }
}

impl Mul<Qty> for Price {
    type Output = Usd;
    fn mul(self, q: Qty) -> Usd {
        self.as_usd() * q
    }
}

impl fmt::Display for Usd {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let a = self.0.unsigned_abs();
        write!(f, "{sign}${}.{:02}", a / 100, a % 100)
    }
}

impl fmt::Display for Price {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.as_usd().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(s: &str) -> Usd {
        Usd::parse(s).unwrap()
    }

    #[test]
    fn parses_alpaca_strings() {
        assert_eq!(usd("25000.00").cents(), 2_500_000);
        assert_eq!(usd("24950.1").cents(), 2_495_010);
        assert_eq!(usd("187").cents(), 18_700);
        assert_eq!(usd("-12.30").cents(), -1_230);
        assert_eq!(usd("0.07").cents(), 7);
        assert_eq!(usd("-0").cents(), 0);
        assert_eq!(usd("-0.004").cents(), 0);
    }

    #[test]
    fn rounds_sub_cent_half_away_from_zero() {
        assert_eq!(usd("1.004").cents(), 100);
        assert_eq!(usd("1.005").cents(), 101);
        assert_eq!(usd("-1.005").cents(), -101);
        assert_eq!(usd("1.999").cents(), 200);
    }

    #[test]
    fn rejects_garbage() {
        for s in [
            "",
            "-",
            ".5",
            "abc",
            "1.2.3",
            "1,000",
            "NaN",
            "+5",
            " 5",
            "1e3",
            "5.",
            "-5.",
            "99999999999999999999",
        ] {
            assert!(Usd::parse(s).is_err(), "{s:?} should be rejected");
        }
    }

    #[test]
    fn price_must_be_positive() {
        assert!(Price::from_cents(0).is_none());
        assert!(Price::from_cents(-1).is_none());
        assert!(Price::parse("0.00").is_err());
        assert_eq!(
            Price::parse("187.33").unwrap(),
            Price::from_cents(18_733).unwrap()
        );
    }

    #[test]
    fn float_prices_round_in_the_stated_direction() {
        let ceil = |x| Price::from_f64_ceil(x).map(Price::cents);
        let floor = |x| Price::from_f64_floor(x).map(Price::cents);
        assert_eq!((ceil(187.33), floor(187.33)), (Some(18_733), Some(18_733)));
        assert_eq!(
            (ceil(187.331), floor(187.339)),
            (Some(18_734), Some(18_733))
        );
        assert_eq!((ceil(0.29), floor(0.29)), (Some(29), Some(29)));
        for bad in [f64::NAN, f64::INFINITY, 0.0, -1.0, 1e20] {
            assert_eq!((ceil(bad), floor(bad)), (None, None), "{bad}");
        }
    }

    #[test]
    fn qty_is_never_zero() {
        assert!(Qty::new(0).is_none());
        assert_eq!(Qty::new(3).unwrap().get(), 3);
    }

    #[test]
    fn sizing_math_is_exact() {
        let entry = Price::from_cents(18_733).unwrap();
        let stop = Price::from_cents(18_600).unwrap();
        let r = risk_per_share(entry, stop).unwrap(); // $1.33 per share
        assert_eq!(r.cents(), 133);
        let by_risk = usd("250").units_of(r); // 25000 / 133 = 187.97 → 187
        let by_notional = usd("2500").units_of(entry.as_usd()); // 250000 / 18733 = 13.3 → 13
        assert_eq!(by_risk.map(Qty::get), Some(187));
        let q = by_risk.min(by_notional).unwrap();
        assert_eq!(q.get(), 13);
        assert_eq!((entry * q).cents(), 243_529);
        assert_eq!((r * q).cents(), 1_729);
    }

    #[test]
    fn risk_per_share_needs_stop_below_entry() {
        let p = |c| Price::from_cents(c).unwrap();
        assert!(risk_per_share(p(100), p(100)).is_none());
        assert!(risk_per_share(p(100), p(101)).is_none());
        assert_eq!(risk_per_share(p(100), p(99)), Some(Usd::from_cents(1)));
    }

    #[test]
    fn units_of_means_no_trade_when_it_cannot_size() {
        assert_eq!(usd("250").units_of(Usd::ZERO), None);
        assert_eq!(usd("250").units_of(usd("-1")), None);
        assert_eq!(usd("-250").units_of(usd("1")), None);
        assert_eq!(usd("0.50").units_of(usd("1")), None); // budget too small
        assert_eq!(Usd::from_cents(i64::MAX).units_of(usd("0.01")), None); // overflows u32
        // either bound failing means no trade: None sorts below Some
        assert_eq!(usd("250").units_of(usd("1")).min(None), None);
    }

    #[test]
    fn arithmetic_saturates_instead_of_wrapping() {
        let huge = Usd::from_cents(i64::MAX);
        assert_eq!((huge + usd("1")).cents(), i64::MAX);
        let q = Qty::new(u32::MAX).unwrap();
        assert_eq!((huge * q).cents(), i64::MAX);
        assert!(huge * q > usd("2500")); // so a notional limit still rejects it
    }

    #[test]
    fn displays_as_dollars() {
        assert_eq!(usd("24950.1").to_string(), "$24950.10");
        assert_eq!(usd("-0.05").to_string(), "-$0.05");
        assert_eq!(
            Usd::from_cents(i64::MIN).to_string(),
            "-$92233720368547758.08"
        );
    }
}
