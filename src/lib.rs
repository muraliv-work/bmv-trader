//! bmv-trader core: shared by the CLI, the backtester and the live engine.

pub mod backtest;
pub mod broker;
pub mod config;
pub mod engine;
pub mod history;
pub mod live;
pub mod market_data;
pub mod momentum;
pub mod money;
pub mod risk;
pub mod safety;
pub mod scan;
pub mod stats;
pub mod strategy;
pub mod swing;
pub mod ui;
pub mod vetting;
pub mod watch;
