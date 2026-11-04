//! The strategy evolution lab (DESIGN §7h, trials 11 and 12): stock-ranking formulas over a monthly
//! feature panel, searched by mutation and crossover. A candidate generator, not a judge: the
//! search only ever sees months that end before `SEARCH_END`, and each later stage runs once.

use crate::momentum::{self, Calendar};
use crate::power;
use crate::scan::DailyBar;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Stage boundaries (DESIGN §7h). A month belongs to a stage when its rebalance is on or after
/// the stage start and its exit (next rebalance) is before the next stage's start, so no label
/// reaches into a later stage.
pub const SEARCH_START: &str = "2016-02-01";
pub const SEARCH_END: &str = "2023-01-01";
pub const VALIDATION_END: &str = "2025-10-02";
/// Fewer eligible names than this and a month's IC isn't counted.
pub const MIN_NAMES: usize = 30;
pub const HOLD: usize = momentum::HOLD;
pub const MAX_DEPTH: usize = 4;
pub const MAX_NODES: usize = 9;
/// Fitness = mean IC − this per node − `TURNOVER_COST` × mean turnover.
pub const NODE_COST: f64 = 0.002;
pub const TURNOVER_COST: f64 = 0.005;
/// Bars needed in the 253 sessions ending at the signal session for a row to exist.
const MIN_BARS: usize = 200;

/// Every input a formula may use; each is computed from bars at or before the signal session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Feature {
    Ret1,
    Ret5,
    Ret21,
    Ret63,
    Ret126,
    Mom12_1,
    Vol21,
    Vol63,
    VolTrend,
    Atr14,
    Range20,
    Range60,
    Ma20,
    Ma50,
    Ma200,
    High252,
    VolRatio20,
    LogDollar63,
    MaxRet21,
    UpDays21,
}

pub const FEATURES: [Feature; 20] = [
    Feature::Ret1,
    Feature::Ret5,
    Feature::Ret21,
    Feature::Ret63,
    Feature::Ret126,
    Feature::Mom12_1,
    Feature::Vol21,
    Feature::Vol63,
    Feature::VolTrend,
    Feature::Atr14,
    Feature::Range20,
    Feature::Range60,
    Feature::Ma20,
    Feature::Ma50,
    Feature::Ma200,
    Feature::High252,
    Feature::VolRatio20,
    Feature::LogDollar63,
    Feature::MaxRet21,
    Feature::UpDays21,
];
const NF: usize = FEATURES.len();

impl Feature {
    fn index(self) -> usize {
        FEATURES.iter().position(|f| *f == self).unwrap_or(0)
    }

    /// One line each, for the local model's prompt.
    pub fn meaning(self) -> &'static str {
        match self {
            Feature::Ret1 => "1-session return",
            Feature::Ret5 => "5-session return",
            Feature::Ret21 => "21-session (1 month) return",
            Feature::Ret63 => "63-session (3 month) return",
            Feature::Ret126 => "126-session (6 month) return",
            Feature::Mom12_1 => "12-1 momentum: return from 252 to 21 sessions ago",
            Feature::Vol21 => "daily return standard deviation, 21 sessions",
            Feature::Vol63 => "daily return standard deviation, 63 sessions",
            Feature::VolTrend => "vol21 / vol63",
            Feature::Atr14 => "14-session average true range / close",
            Feature::Range20 => "position of the close in the 20-session high-low range, 0..1",
            Feature::Range60 => "position of the close in the 60-session high-low range, 0..1",
            Feature::Ma20 => "close / 20-session moving average - 1",
            Feature::Ma50 => "close / 50-session moving average - 1",
            Feature::Ma200 => "close / 200-session moving average - 1",
            Feature::High252 => "close / 252-session high - 1",
            Feature::VolRatio20 => "last volume / 20-session average volume",
            Feature::LogDollar63 => "log of 63-session average dollar volume",
            Feature::MaxRet21 => "largest 1-session return in 21 sessions",
            Feature::UpDays21 => "share of up sessions in 21",
        }
    }
}

/// The features of one symbol at signal session `s`, or None if it has too little history.
/// Reads `on[..=s]` only: nothing on or after the rebalance session.
pub fn features(on: &[Option<&DailyBar>], s: usize) -> Option<[f32; NF]> {
    on.get(s)?.as_ref()?;
    let w: Vec<&DailyBar> = on[s.saturating_sub(252)..=s]
        .iter()
        .flatten()
        .copied()
        .collect();
    if w.len() < MIN_BARS {
        return None;
    }
    let n = w.len() - 1;
    let c = |i: usize| w[n - i].close;
    let ret = |k: usize| c(0) / c(k.min(n)) - 1.0;
    let rets = |k: usize| -> Vec<f64> { (0..k.min(n)).map(|i| c(i) / c(i + 1) - 1.0).collect() };
    let sd = |k: usize| power::sd(&rets(k));
    let ma = |k: usize| {
        c(0) / (w[n + 1 - k.min(n + 1)..]
            .iter()
            .map(|b| b.close)
            .sum::<f64>()
            / k.min(n + 1) as f64)
            - 1.0
    };
    let range = |k: usize| {
        let x = &w[n + 1 - k.min(n + 1)..];
        let hi = x.iter().map(|b| b.high).fold(f64::MIN, f64::max);
        let lo = x.iter().map(|b| b.low).fold(f64::MAX, f64::min);
        if hi > lo {
            (c(0) - lo) / (hi - lo)
        } else {
            f64::NAN
        }
    };
    let tr: Vec<f64> = (0..14.min(n))
        .map(|i| {
            let (b, prev) = (w[n - i], w[n - i - 1].close);
            (b.high - b.low)
                .max((b.high - prev).abs())
                .max((b.low - prev).abs())
        })
        .collect();
    let vol20 = w[n.saturating_sub(19)..]
        .iter()
        .map(|b| b.volume as f64)
        .sum::<f64>()
        / 20f64.min((n + 1) as f64);
    let dollar63 = w[n.saturating_sub(62)..]
        .iter()
        .map(|b| b.close * b.volume as f64)
        .sum::<f64>()
        / 63f64.min((n + 1) as f64);
    // the same 12-1 momentum as trial 7, by session, not by bar count
    let mom = match (
        on.get(s.wrapping_sub(momentum::SKIP)),
        on.get(s.wrapping_sub(momentum::LOOKBACK)),
    ) {
        (Some(Some(r)), Some(Some(p))) if s >= momentum::LOOKBACK => r.close / p.close - 1.0,
        _ => f64::NAN,
    };
    let rets21 = rets(21);
    let v = [
        ret(1),
        ret(5),
        ret(21),
        ret(63),
        ret(126),
        mom,
        sd(21),
        sd(63),
        sd(21) / sd(63),
        momentum::mean(&tr) / c(0),
        range(20),
        range(60),
        ma(20),
        ma(50),
        ma(200),
        c(0) / w.iter().map(|b| b.high).fold(f64::MIN, f64::max) - 1.0,
        w[n].volume as f64 / vol20,
        dollar63.max(1.0).ln(),
        rets21.iter().copied().fold(f64::MIN, f64::max),
        rets21.iter().filter(|r| **r > 0.0).count() as f64 / rets21.len().max(1) as f64,
    ];
    Some(v.map(|x| if x.is_finite() { x as f32 } else { f32::NAN }))
}

/// One eligible name in one month.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub sym: u32,
    pub label: f64,
    pub f: [f32; NF],
}

/// Rows per rebalance month, only for months whose exit is before `until`.
#[derive(Debug, Default)]
pub struct Panel {
    pub symbols: Vec<String>,
    pub dates: Vec<String>,
    /// Indexed like `Calendar::rebalances`; months at or past `until` stay empty.
    pub months: Vec<Vec<Row>>,
    pub until: String,
}

impl Panel {
    pub fn new(cal: &Calendar, until: &str) -> Panel {
        Panel {
            symbols: Vec::new(),
            dates: (0..cal.rebalances.len())
                .map(|k| cal.date(k).to_string())
                .collect(),
            months: vec![Vec::new(); cal.rebalances.len()],
            until: until.to_string(),
        }
    }

    /// Adds one symbol's rows: liquid at the signal session, with a label whose exit is before
    /// `until`. Labels past `until` are never computed.
    pub fn add(&mut self, symbol: &str, bars: &[DailyBar], cal: &Calendar) {
        let on = cal.aligned(bars);
        let sym = self.symbols.len() as u32;
        let mut any = false;
        for k in 0..cal.rebalances.len().saturating_sub(1) {
            let (r, next) = (cal.rebalances[k], cal.rebalances[k + 1]);
            if cal.sessions[next].as_str() >= self.until.as_str() || r == 0 {
                continue;
            }
            if !momentum::liquid(&on, r - 1) {
                continue;
            }
            let (Some(f), Some(label)) =
                (features(&on, r - 1), momentum::held_return(&on, r, next))
            else {
                continue;
            };
            self.months[k].push(Row { sym, label, f });
            any = true;
        }
        if any {
            self.symbols.push(symbol.to_string());
        }
    }

    /// Month indices from `from` (inclusive rebalance date) with an exit before `to`.
    pub fn range(&self, cal: &Calendar, from: &str, to: &str) -> Vec<usize> {
        (0..self.months.len())
            .filter(|&k| self.dates[k].as_str() >= from)
            .filter(|&k| cal.exit_date(k).is_some_and(|d| d < to) && to <= self.until.as_str())
            .collect()
    }
}

/// A ranking formula. Typed, never code: LLM output enters only by parsing as this (DECISION S25).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Expr {
    Feat(Feature),
    Rank(Box<Expr>),
    Neg(Box<Expr>),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
    Div(Box<Expr>, Box<Expr>),
}

impl Expr {
    pub fn nodes(&self) -> usize {
        match self {
            Expr::Feat(_) => 1,
            Expr::Rank(a) | Expr::Neg(a) => 1 + a.nodes(),
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                1 + a.nodes() + b.nodes()
            }
        }
    }

    pub fn depth(&self) -> usize {
        match self {
            Expr::Feat(_) => 1,
            Expr::Rank(a) | Expr::Neg(a) => 1 + a.depth(),
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
                1 + a.depth().max(b.depth())
            }
        }
    }

    pub fn fits(&self) -> bool {
        self.depth() <= MAX_DEPTH && self.nodes() <= MAX_NODES
    }

    /// Strict JSON, then the size caps.
    pub fn parse(text: &str) -> Result<Expr, String> {
        let e: Expr = serde_json::from_str(text).map_err(|e| format!("not a formula: {e}"))?;
        if !e.fits() {
            return Err(format!(
                "too big: depth {} (max {MAX_DEPTH}), {} nodes (max {MAX_NODES})",
                e.depth(),
                e.nodes()
            ));
        }
        Ok(e)
    }

    /// A canonical text form, for dropping duplicates.
    pub fn key(&self) -> String {
        let f = |e: &Expr| e.key();
        match self {
            Expr::Feat(x) => serde_json::to_string(x)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string(),
            Expr::Rank(a) => format!("rank({})", f(a)),
            Expr::Neg(a) => format!("-({})", f(a)),
            // + and × don't care about order
            Expr::Add(a, b) | Expr::Mul(a, b) => {
                let (x, y) = (f(a), f(b));
                let (x, y) = if x <= y { (x, y) } else { (y, x) };
                format!(
                    "({x} {} {y})",
                    if matches!(self, Expr::Add(..)) {
                        "+"
                    } else {
                        "*"
                    }
                )
            }
            Expr::Sub(a, b) => format!("({} - {})", f(a), f(b)),
            Expr::Div(a, b) => format!("({} / {})", f(a), f(b)),
        }
    }

    /// Scores for every row of one month; NaN where an input is missing.
    pub fn eval(&self, rows: &[Row]) -> Vec<f64> {
        let both = |a: &Expr, b: &Expr, op: &dyn Fn(f64, f64) -> f64| -> Vec<f64> {
            a.eval(rows)
                .into_iter()
                .zip(b.eval(rows))
                .map(|(x, y)| op(x, y))
                .collect()
        };
        match self {
            Expr::Feat(x) => rows.iter().map(|r| r.f[x.index()] as f64).collect(),
            Expr::Rank(a) => rank_scaled(&a.eval(rows)),
            Expr::Neg(a) => a.eval(rows).into_iter().map(|x| -x).collect(),
            Expr::Add(a, b) => both(a, b, &|x, y| x + y),
            Expr::Sub(a, b) => both(a, b, &|x, y| x - y),
            Expr::Mul(a, b) => both(a, b, &|x, y| x * y),
            Expr::Div(a, b) => both(a, b, &|x, y| if y.abs() > 1e-9 { x / y } else { f64::NAN }),
        }
    }
}

/// Cross-sectional rank scaled to −1..1 (ties share); NaN stays NaN.
fn rank_scaled(x: &[f64]) -> Vec<f64> {
    let idx: Vec<usize> = (0..x.len()).filter(|&i| x[i].is_finite()).collect();
    let vals: Vec<f64> = idx.iter().map(|&i| x[i]).collect();
    let r = power::ranks(&vals);
    let mut out = vec![f64::NAN; x.len()];
    let n = vals.len().max(2) as f64;
    for (j, &i) in idx.iter().enumerate() {
        out[i] = 2.0 * (r[j] - 1.0) / (n - 1.0) - 1.0;
    }
    out
}

/// One month's result for one formula.
#[derive(Clone, Debug, PartialEq)]
pub struct MonthScore {
    pub ic: Option<f64>,
    /// Top-20 net mean return minus the cross-sectional mean (the expected random-20) net of a
    /// full turnover.
    pub excess: f64,
    pub turnover: f64,
    pub top: Vec<u32>,
}

/// Scores one month; `prev` is last month's top 20 (for turnover and costs).
pub fn score_month(e: &Expr, rows: &[Row], prev: &[u32]) -> Option<MonthScore> {
    let s = e.eval(rows);
    let ok: Vec<usize> = (0..rows.len()).filter(|&i| s[i].is_finite()).collect();
    if ok.len() < MIN_NAMES {
        return None;
    }
    let sc: Vec<f64> = ok.iter().map(|&i| s[i]).collect();
    let lab: Vec<f64> = ok.iter().map(|&i| rows[i].label).collect();
    let ic = power::spearman(&sc, &lab);
    let mut order: Vec<usize> = ok.clone();
    order.sort_by(|&a, &b| s[b].total_cmp(&s[a]).then(rows[a].sym.cmp(&rows[b].sym)));
    order.truncate(HOLD);
    let top: Vec<u32> = order.iter().map(|&i| rows[i].sym).collect();
    let kept = top.iter().filter(|x| prev.contains(x)).count();
    let turnover = if prev.is_empty() {
        1.0
    } else {
        1.0 - kept as f64 / HOLD as f64
    };
    let cost = 2.0 * momentum::COST_PER_SIDE;
    let held = order.iter().map(|&i| rows[i].label).sum::<f64>() / order.len() as f64;
    let all = momentum::mean(&lab);
    Some(MonthScore {
        ic,
        excess: (held - cost * turnover) - (all - cost),
        turnover,
        top,
    })
}

/// A formula over a set of months.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub months: usize,
    pub ic_mean: f64,
    pub ic_sd: f64,
    pub excess_mean: f64,
    pub turnover: f64,
    pub nodes: usize,
    pub fitness: f64,
    /// Monthly ICs in month order (not written to the log).
    #[serde(skip)]
    pub ics: Vec<f64>,
}

pub fn evaluate(e: &Expr, panel: &Panel, months: &[usize]) -> Option<Stats> {
    let (mut ics, mut ex, mut to) = (Vec::new(), Vec::new(), Vec::new());
    let mut prev: Vec<u32> = Vec::new();
    for &k in months {
        if let Some(m) = score_month(e, &panel.months[k], &prev) {
            if let Some(ic) = m.ic {
                ics.push(ic);
            }
            ex.push(m.excess);
            to.push(m.turnover);
            prev = m.top;
        }
    }
    if ics.len() < 12 {
        return None;
    }
    let ic_mean = momentum::mean(&ics);
    let turnover = momentum::mean(&to);
    let excess_mean = momentum::mean(&ex);
    let nodes = e.nodes();
    Some(Stats {
        months: ics.len(),
        ic_mean,
        ic_sd: power::sd(&ics),
        excess_mean,
        turnover,
        nodes,
        fitness: ic_mean - NODE_COST * nodes as f64 - TURNOVER_COST * turnover,
        ics,
    })
}

/// Trial 11's name in `data/trials.jsonl` (DESIGN §7h).
pub const TRIAL_11: &str = "lab ranking search";
/// Trial 11 is judged at 1 − 0.05/11 whatever else is logged.
pub const TRIAL_11_NUMBER: usize = 11;
/// How many search candidates go to validation, how distinct they must be, and how many advance.
pub const VALIDATE: usize = 20;
pub const MAX_SCORE_CORR: f64 = 0.9;
pub const ADVANCE: usize = 3;

/// The lower bound of the mean IC: two-sided at 1 − 0.05/`trials`, resampling 3-month blocks.
pub fn ic_bound(ics: &[f64], trials: usize) -> Option<f64> {
    let alpha = crate::stats::FAMILY_ALPHA / trials.max(1) as f64;
    let blocks: Vec<Vec<f64>> = ics
        .chunks(momentum::BLOCK_MONTHS)
        .map(<[f64]>::to_vec)
        .collect();
    crate::stats::bootstrap_expectancy(&blocks, crate::stats::resamples_for(alpha), alpha, 42)
        .map(|(lo, _)| lo)
}

/// The mean monthly rank correlation of two formulas' scores over `months`.
pub fn score_corr(a: &Expr, b: &Expr, panel: &Panel, months: &[usize]) -> f64 {
    let cs: Vec<f64> = months
        .iter()
        .filter_map(|&k| {
            let rows = &panel.months[k];
            let (x, y) = (a.eval(rows), b.eval(rows));
            let both: Vec<usize> = (0..rows.len())
                .filter(|&i| x[i].is_finite() && y[i].is_finite())
                .collect();
            power::spearman(
                &both.iter().map(|&i| x[i]).collect::<Vec<_>>(),
                &both.iter().map(|&i| y[i]).collect::<Vec<_>>(),
            )
        })
        .collect();
    if cs.is_empty() {
        1.0
    } else {
        momentum::mean(&cs)
    }
}

/// The best `want` candidates by search fitness whose top 20 beat random, each scoring stocks
/// differently from every one kept before it (|correlation| at most `MAX_SCORE_CORR` on `months`).
pub fn distinct(
    cands: &[Candidate],
    panel: &Panel,
    months: &[usize],
    want: usize,
) -> Vec<Candidate> {
    let mut sorted: Vec<&Candidate> = cands.iter().filter(|c| c.stats.excess_mean > 0.0).collect();
    sorted.sort_by(|a, b| {
        b.stats
            .fitness
            .total_cmp(&a.stats.fitness)
            .then(a.key.cmp(&b.key))
    });
    let mut keys: HashSet<&str> = HashSet::new();
    let mut kept: Vec<Candidate> = Vec::new();
    for c in sorted {
        if kept.len() >= want {
            break;
        }
        if !keys.insert(c.key.as_str()) {
            continue;
        }
        if kept
            .iter()
            .all(|k| score_corr(&k.expr, &c.expr, panel, months).abs() <= MAX_SCORE_CORR)
        {
            kept.push(c.clone());
        }
    }
    kept
}

/// The best mean IC expected by luck from `tries` formulas with no edge (DESIGN §7h).
pub fn luck_line(ic_sd: f64, months: usize, tries: usize) -> f64 {
    ic_sd / (months.max(1) as f64).sqrt() * (2.0 * (tries.max(2) as f64).ln()).sqrt()
}

/// xorshift64: the same seed always gives the same search.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).max(1))
    }
    pub fn draw(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.draw() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, p: f64) -> bool {
        (self.draw() % 10_000) as f64 / 10_000.0 < p
    }
}

pub fn random_expr(rng: &mut Rng, depth: usize) -> Expr {
    let leaf = |rng: &mut Rng| Expr::Feat(FEATURES[rng.below(NF)]);
    if depth <= 1 || rng.chance(0.3) {
        return leaf(rng);
    }
    let sub = |rng: &mut Rng| Box::new(random_expr(rng, depth - 1));
    match rng.below(6) {
        0 => Expr::Rank(sub(rng)),
        1 => Expr::Neg(sub(rng)),
        2 => Expr::Add(sub(rng), sub(rng)),
        3 => Expr::Sub(sub(rng), sub(rng)),
        4 => Expr::Mul(sub(rng), sub(rng)),
        _ => Expr::Div(sub(rng), sub(rng)),
    }
}

/// Every subtree, depth first, as a path of child choices.
fn paths(e: &Expr, at: Vec<u8>, out: &mut Vec<Vec<u8>>) {
    out.push(at.clone());
    let mut go = |c: &Expr, i: u8| {
        let mut p = at.clone();
        p.push(i);
        paths(c, p, out);
    };
    match e {
        Expr::Feat(_) => {}
        Expr::Rank(a) | Expr::Neg(a) => go(a, 0),
        Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
            go(a, 0);
            go(b, 1);
        }
    }
}

fn at<'a>(e: &'a Expr, path: &[u8]) -> &'a Expr {
    match (e, path.first()) {
        (_, None) | (Expr::Feat(_), _) => e,
        (Expr::Rank(a) | Expr::Neg(a), _) => at(a, &path[1..]),
        (Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b), Some(&i)) => {
            at(if i == 0 { a } else { b }, &path[1..])
        }
    }
}

fn replace(e: &Expr, path: &[u8], with: &Expr) -> Expr {
    let Some(&i) = path.first() else {
        return with.clone();
    };
    let r = |c: &Expr| Box::new(replace(c, &path[1..], with));
    match e {
        Expr::Feat(_) => with.clone(),
        Expr::Rank(a) => Expr::Rank(r(a)),
        Expr::Neg(a) => Expr::Neg(r(a)),
        Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) | Expr::Div(a, b) => {
            let (a, b) = if i == 0 {
                (r(a), b.clone())
            } else {
                (a.clone(), r(b))
            };
            match e {
                Expr::Add(..) => Expr::Add(a, b),
                Expr::Sub(..) => Expr::Sub(a, b),
                Expr::Mul(..) => Expr::Mul(a, b),
                _ => Expr::Div(a, b),
            }
        }
    }
}

/// A child of `a` (and `b`, for crossover) that still fits the caps; None if a few tries fail.
pub fn child(a: &Expr, b: &Expr, rng: &mut Rng) -> Option<Expr> {
    for _ in 0..8 {
        let mut pa = Vec::new();
        paths(a, Vec::new(), &mut pa);
        let p = &pa[rng.below(pa.len())];
        let c = match rng.below(5) {
            // crossover: a subtree of b in place of one of a
            0 => {
                let mut pb = Vec::new();
                paths(b, Vec::new(), &mut pb);
                replace(a, p, at(b, &pb[rng.below(pb.len())]))
            }
            1 => replace(a, p, &random_expr(rng, 2)),
            2 => replace(a, p, &Expr::Rank(Box::new(at(a, p).clone()))),
            3 => replace(a, p, &Expr::Neg(Box::new(at(a, p).clone()))),
            // prune: a subtree takes its parent's place
            _ => {
                let sub = at(a, p);
                let mut ps = Vec::new();
                paths(sub, Vec::new(), &mut ps);
                at(sub, &ps[rng.below(ps.len())]).clone()
            }
        };
        if c.fits() && c != *a {
            return Some(c);
        }
    }
    None
}

/// One evaluated candidate, as logged in `data/lab/candidates.jsonl`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub key: String,
    pub expr: Expr,
    pub source: String,
    pub seed: u64,
    pub stats: Stats,
}

/// Evolves for `budget` tries from `seeds` (earlier bests and ideas) plus random formulas, on
/// `months` only. `log` gets every evaluated candidate; returns the population, best first.
pub fn search(
    panel: &Panel,
    months: &[usize],
    seeds: Vec<(Expr, String)>,
    seed: u64,
    budget: &mut dyn FnMut(usize) -> bool,
    log: &mut dyn FnMut(&Candidate),
) -> (Vec<Candidate>, usize) {
    const POP: usize = 200;
    // children per round: made in order, scored in parallel, applied in order, so a seed always
    // gives the same search however the threads finish
    const BATCH: usize = 64;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut rng = Rng::new(seed);
    let mut seen: HashSet<String> = HashSet::new();
    let mut pop: Vec<Candidate> = Vec::new();
    let mut tries = 0;
    let mut queue: Vec<(Expr, String)> = seeds.into_iter().filter(|(e, _)| e.fits()).collect();
    queue.reverse();
    while budget(tries) {
        let mut batch: Vec<(Expr, String)> = Vec::new();
        let mut attempts = 0;
        while batch.len() < BATCH && attempts < BATCH * 4 {
            attempts += 1;
            let next = if let Some(s) = queue.pop() {
                Some(s)
            } else if pop.len() < POP / 2 || rng.chance(0.1) {
                Some((random_expr(&mut rng, 3), "random".to_string()))
            } else {
                let pick = |rng: &mut Rng| -> usize {
                    (0..4)
                        .map(|_| rng.below(pop.len()))
                        .max_by(|&a, &b| pop[a].stats.fitness.total_cmp(&pop[b].stats.fitness))
                        .unwrap_or(0)
                };
                let (a, b) = (pick(&mut rng), pick(&mut rng));
                child(&pop[a].expr, &pop[b].expr, &mut rng).map(|e| (e, "mutation".to_string()))
            };
            if let Some((e, source)) = next
                && seen.insert(e.key())
            {
                batch.push((e, source));
            }
        }
        // count failed breeding too, so a stuck population still ends
        tries += batch.len().max(1);
        let chunk = batch.len().div_ceil(threads).max(1);
        let scored: Vec<Option<Stats>> = std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .chunks(chunk)
                .map(|part| {
                    s.spawn(move || {
                        part.iter()
                            .map(|(e, _)| evaluate(e, panel, months))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect()
        });
        for ((e, source), stats) in batch.into_iter().zip(scored) {
            let Some(stats) = stats else { continue };
            let c = Candidate {
                key: e.key(),
                expr: e,
                source,
                seed,
                stats,
            };
            log(&c);
            // a candidate counts only if its top 20 beats random after costs
            if c.stats.excess_mean <= 0.0 {
                continue;
            }
            if pop.len() < POP {
                pop.push(c);
            } else if let Some((w, worst)) = pop
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.stats.fitness.total_cmp(&b.1.stats.fitness))
                && c.stats.fitness > worst.stats.fitness
            {
                pop[w] = c;
            }
        }
    }
    pop.sort_by(|a, b| b.stats.fitness.total_cmp(&a.stats.fitness));
    (pop, tries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bar(date: &str, close: f64, volume: u64) -> DailyBar {
        DailyBar {
            date: date.into(),
            open: close,
            high: close * 1.01,
            low: close * 0.99,
            close,
            volume,
        }
    }

    /// A synthetic panel: `months` months of `names` rows with 20 features; feature 0 is
    /// whatever `planted` says about the label.
    fn panel(
        months: usize,
        names: usize,
        seed: u64,
        planted: &dyn Fn(f64, &mut Rng) -> f32,
    ) -> Panel {
        let mut rng = Rng::new(seed);
        let mut p = Panel {
            until: "9999".into(),
            ..Panel::default()
        };
        for k in 0..months {
            p.dates.push(format!("m{k:03}"));
            let rows = (0..names)
                .map(|i| {
                    let label = (rng.below(2001) as f64 - 1000.0) / 10_000.0;
                    let mut f = [0f32; NF];
                    for x in f.iter_mut() {
                        *x = (rng.below(1000) as f32) / 1000.0;
                    }
                    f[0] = planted(label, &mut rng);
                    Row {
                        sym: i as u32,
                        label,
                        f,
                    }
                })
                .collect();
            p.months.push(rows);
        }
        p
    }

    #[test]
    fn features_read_nothing_on_or_after_the_signal_session() {
        let bars: Vec<DailyBar> = (0..300)
            .map(|i| bar(&format!("d{i:03}"), 100.0 + i as f64, 1_000_000))
            .collect();
        let on: Vec<Option<&DailyBar>> = bars.iter().map(Some).collect();
        let s = 280;
        let f = features(&on, s).unwrap();
        // change everything after s: the features at s must not move
        let mut later = bars.clone();
        for b in later.iter_mut().skip(s + 1) {
            *b = bar(&b.date.clone(), 1.0, 1);
        }
        let on2: Vec<Option<&DailyBar>> = later.iter().map(Some).collect();
        let f2 = features(&on2, s).unwrap();
        for i in 0..NF {
            assert!(
                f[i] == f2[i] || (f[i].is_nan() && f2[i].is_nan()),
                "feature {i}"
            );
        }
        assert!((f[Feature::Ret1.index()] as f64 - (380.0 / 379.0 - 1.0)).abs() < 1e-6);
        assert!(features(&on, 150).is_none(), "too little history");
    }

    #[test]
    fn the_parser_takes_only_formulas_within_the_caps() {
        let ok = Expr::parse(r#"{"sub":[{"rank":{"feat":"mom12_1"}},{"feat":"vol63"}]}"#).unwrap();
        assert_eq!(ok.key(), "(rank(mom12_1) - vol63)");
        for bad in [
            r#"{"feat":"next_month_return"}"#,
            r#"{"rank":{"feat":"ret5"},"extra":1}"#,
            r#"{"exec":"rm -rf /"}"#,
            r#"{"rank":{"rank":{"rank":{"rank":{"feat":"ret5"}}}}}"#,
            r#"not json"#,
        ] {
            assert!(Expr::parse(bad).is_err(), "{bad}");
        }
        let big = r#"{"add":[{"add":[{"feat":"ret1"},{"feat":"ret5"}]},{"add":[{"add":[{"feat":"ret21"},{"feat":"ret63"}]},{"mul":[{"feat":"ret126"},{"feat":"vol21"}]}]}]}"#;
        assert!(Expr::parse(big).is_err(), "more than 9 nodes");
        let a = Expr::parse(r#"{"add":[{"feat":"ret1"},{"feat":"ret5"}]}"#).unwrap();
        let b = Expr::parse(r#"{"add":[{"feat":"ret5"},{"feat":"ret1"}]}"#).unwrap();
        assert_eq!(a.key(), b.key(), "order doesn't matter for +");
    }

    #[test]
    fn a_planted_future_feature_is_found_and_a_shifted_one_is_not() {
        let months: Vec<usize> = (0..40).collect();
        let leak = panel(40, 200, 1, &|label, _| label as f32);
        let s = evaluate(&Expr::Feat(Feature::Ret1), &leak, &months).unwrap();
        assert!(s.ic_mean > 0.99, "{}", s.ic_mean);
        assert!(s.excess_mean > 0.0);
        // feature 0 holds last month's label: no information about this month
        let mut shifted = panel(40, 200, 1, &|label, _| label as f32);
        for k in (1..40).rev() {
            let prev: Vec<f32> = shifted.months[k - 1]
                .iter()
                .map(|r| r.label as f32)
                .collect();
            for (r, p) in shifted.months[k].iter_mut().zip(prev) {
                r.f[0] = p;
            }
        }
        let s = evaluate(&Expr::Feat(Feature::Ret1), &shifted, &months[1..]).unwrap();
        assert!(s.ic_mean.abs() < 0.05, "{}", s.ic_mean);
    }

    #[test]
    fn search_on_noise_finds_only_luck() {
        let noise = panel(60, 150, 7, &|_, rng| rng.below(1000) as f32);
        let months: Vec<usize> = (0..60).collect();
        let mut logged = 0;
        let (pop, tries) = search(&noise, &months, vec![], 3, &mut |t| t < 1500, &mut |_| {
            logged += 1
        });
        assert!(tries >= 1500 && logged > 0);
        let best = pop.first().map_or(0.0, |c| c.stats.ic_mean);
        let line = luck_line(0.08, 60, tries);
        assert!(best < 2.0 * line, "best {best} vs luck line {line}");
        // and it doesn't carry over: the same formula on fresh noise
        let fresh = panel(60, 150, 99, &|_, rng| rng.below(1000) as f32);
        if let Some(c) = pop.first() {
            let s = evaluate(&c.expr, &fresh, &months).unwrap();
            assert!(s.ic_mean.abs() < 0.03, "{}", s.ic_mean);
        }
    }

    #[test]
    fn the_same_seed_gives_the_same_search() {
        let noise = panel(30, 80, 4, &|_, rng| rng.below(1000) as f32);
        let months: Vec<usize> = (0..30).collect();
        let run = || {
            let (pop, tries) = search(&noise, &months, vec![], 9, &mut |t| t < 600, &mut |_| {});
            (pop.iter().map(|c| c.key.clone()).collect::<Vec<_>>(), tries)
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn distinct_picks_drop_near_copies_and_the_bound_is_strict() {
        let p = panel(24, 100, 6, &|label, _| label as f32);
        let months: Vec<usize> = (0..24).collect();
        let c = |e: Expr, fitness: f64| {
            let mut stats = evaluate(&e, &p, &months).unwrap();
            stats.fitness = fitness;
            Candidate {
                key: e.key(),
                expr: e,
                source: "t".into(),
                seed: 0,
                stats,
            }
        };
        let a = Expr::Feat(Feature::Ret1);
        let copy = Expr::Rank(Box::new(Expr::Feat(Feature::Ret1)));
        let other = Expr::Feat(Feature::Ret5);
        let mut other_c = c(other, 0.1);
        other_c.stats.excess_mean = 0.01;
        let picks = distinct(&[c(a, 0.3), c(copy, 0.2), other_c], &p, &months, 20);
        assert_eq!(
            picks.iter().map(|x| x.key.as_str()).collect::<Vec<_>>(),
            ["ret1", "ret5"]
        );
        assert!(ic_bound(&[0.5; 30], 11).unwrap() > 0.4);
        let noise: Vec<f64> = (0..30)
            .map(|i| if i % 2 == 0 { 0.1 } else { -0.1 })
            .collect();
        assert!(ic_bound(&noise, 11).unwrap() < 0.0);
    }

    #[test]
    fn search_finds_a_planted_signal() {
        let signal = panel(48, 150, 5, &|label, rng| {
            (label + (rng.below(1000) as f64 - 500.0) / 20_000.0) as f32
        });
        let months: Vec<usize> = (0..48).collect();
        let (pop, _) = search(&signal, &months, vec![], 11, &mut |t| t < 1500, &mut |_| {});
        assert!(pop[0].stats.ic_mean > 0.3, "{}", pop[0].key);
    }

    #[test]
    fn children_stay_within_the_caps_and_ranks_are_scaled() {
        let mut rng = Rng::new(2);
        let a = random_expr(&mut rng, 4);
        let b = random_expr(&mut rng, 4);
        for _ in 0..500 {
            if let Some(c) = child(&a, &b, &mut rng) {
                assert!(c.fits());
            }
        }
        let r = rank_scaled(&[3.0, f64::NAN, 1.0, 2.0]);
        assert_eq!((r[0], r[2], r[3]), (1.0, -1.0, 0.0));
        assert!(r[1].is_nan());
        assert!(luck_line(0.1, 100, 10_000) > luck_line(0.1, 100, 10));
    }
}
