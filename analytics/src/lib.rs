//! Offline performance analytics over run logs (P2-2).
//!
//! Reads `data/run-*.jsonl` via `persistence`, reconstructs closed
//! trades with broker-identical average-cost matching, and reports
//! per-agent + portfolio metrics plus a demo→live promotion gate.
//! Pure offline computation — never touches the trading hot path.

pub mod gate;
pub mod metrics;
pub mod reconstruct;
pub mod report;

pub use gate::{evaluate, GateResult, Thresholds};
pub use metrics::TradeStats;
pub use reconstruct::ClosedTrade;
pub use report::{analyze, AgentReport, PortfolioReport, Report};
