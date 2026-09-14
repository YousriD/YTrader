//! Demo→live promotion gate (P2-2).
//!
//! Thresholds are starting points for a personal paper lab, not
//! recommendations — tune them per venue, symbol, and sample size, and
//! never promote on a green gate alone without reading the full report.
//! Every failure names its rule so a red gate teaches instead of just
//! blocking.

use serde::Serialize;

use crate::report::AgentReport;

/// Promotion thresholds. Defaults encode "show me a real sample with
/// bounded downside and non-negative expectancy geometry":
/// 20+ closed trades, non-negative profit geometry, ≤25% drawdown.
#[derive(Debug, Clone)]
pub struct Thresholds {
    pub min_trades: usize,
    pub min_win_rate_pct: f64,
    pub min_profit_factor: f64,
    pub max_drawdown_pct: f64,
    pub min_sharpe_per_tick: f64,
    pub min_ticks: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_trades: 20,
            min_win_rate_pct: 0.0, // trend systems win <50% legitimately
            min_profit_factor: 1.0,
            max_drawdown_pct: 25.0,
            min_sharpe_per_tick: 0.0,
            min_ticks: 200,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GateResult {
    pub agent_id: String,
    pub pass: bool,
    pub failures: Vec<String>,
}

pub fn evaluate(report: &AgentReport, t: &Thresholds) -> GateResult {
    let mut failures = Vec::new();
    if report.ticks < t.min_ticks {
        failures.push(format!("only {} ticks observed (< {})", report.ticks, t.min_ticks));
    }
    if report.trade_stats.trades < t.min_trades {
        failures.push(format!(
            "only {} closed trades (< {})",
            report.trade_stats.trades, t.min_trades
        ));
    }
    if report.trade_stats.win_rate_pct < t.min_win_rate_pct {
        failures.push(format!(
            "win rate {:.1}% (< {:.1}%)",
            report.trade_stats.win_rate_pct, t.min_win_rate_pct
        ));
    }
    match report.trade_stats.profit_factor {
        Some(pf) if pf < t.min_profit_factor => {
            failures.push(format!("profit factor {pf:.2} (< {:.2})", t.min_profit_factor))
        }
        None if report.trade_stats.trades >= t.min_trades => {
            // All-win run: ratio undefined, geometry non-negative — pass
            // with the sample-size gate already satisfied above.
        }
        None => failures.push("profit factor undefined (no losing trades in a tiny sample)".to_string()),
        _ => {}
    }
    if report.max_drawdown_pct > t.max_drawdown_pct {
        failures.push(format!(
            "max drawdown {:.1}% (> {:.1}%)",
            report.max_drawdown_pct, t.max_drawdown_pct
        ));
    }
    if report.sharpe_per_tick < t.min_sharpe_per_tick {
        failures.push(format!(
            "sharpe {:.3} (< {:.3})",
            report.sharpe_per_tick, t.min_sharpe_per_tick
        ));
    }
    if report.died {
        failures.push("agent died (equity hit zero)".to_string());
    }
    GateResult { agent_id: report.agent_id.clone(), pass: failures.is_empty(), failures }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::TradeStats;
    use crate::report::AgentReport;

    fn report(trades: usize, pf: Option<f64>, dd: f64, ticks: u32, died: bool) -> AgentReport {
        AgentReport {
            agent_id: "a".to_string(),
            ticks,
            time_in_market_pct: 50.0,
            trade_stats: TradeStats {
                trades,
                wins: trades,
                win_rate_pct: 60.0,
                profit_factor: pf,
                avg_win: 5.0,
                avg_loss: 2.0,
                expectancy: 3.0,
            },
            max_drawdown_pct: dd,
            sharpe_per_tick: 0.1,
            sortino_per_tick: None,
            final_equity: 120.0,
            total_withdrawn: 0.0,
            total_return_pct: 20.0,
            splits: 0,
            died,
            unmatched_closes: 0,
            skipped: 0,
        }
    }

    #[test]
    fn failing_sample_lists_every_broken_rule() {
        let r = evaluate(&report(3, Some(0.5), 40.0, 50, true), &Thresholds::default());
        assert!(!r.pass);
        assert!(r.failures.iter().any(|f| f.contains("closed trades")));
        assert!(r.failures.iter().any(|f| f.contains("profit factor")));
        assert!(r.failures.iter().any(|f| f.contains("drawdown")));
        assert!(r.failures.iter().any(|f| f.contains("died")));
        assert!(r.failures.iter().any(|f| f.contains("ticks observed")));
    }

    #[test]
    fn all_win_run_passes_profit_geometry() {
        let r = evaluate(&report(25, None, 5.0, 300, false), &Thresholds::default());
        assert!(r.pass, "failures: {:?}", r.failures);
    }

    #[test]
    fn death_alone_fails_an_otherwise_green_run() {
        let r = evaluate(&report(25, Some(1.5), 5.0, 300, true), &Thresholds::default());
        assert!(!r.pass);
        assert_eq!(r.failures.len(), 1);
    }
}
