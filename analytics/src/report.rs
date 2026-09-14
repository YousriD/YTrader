//! Report assembly: per-agent + portfolio views over a run log (P2-2).

use std::collections::BTreeMap;

use persistence::EventRecord;
use serde::Serialize;

use crate::metrics::{max_drawdown_pct, sharpe_per_tick, sortino_per_tick, value_curve, TradeStats};
use crate::reconstruct::reconstruct;

fn num(data: &serde_json::Value, key: &str) -> Option<f64> {
    data.get(key)?.as_f64()
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentReport {
    pub agent_id: String,
    pub ticks: u32,
    pub time_in_market_pct: f64,
    pub trade_stats: TradeStats,
    pub max_drawdown_pct: f64,
    pub sharpe_per_tick: f64,
    pub sortino_per_tick: Option<f64>,
    pub final_equity: f64,
    pub total_withdrawn: f64,
    pub total_return_pct: f64,
    pub splits: u32,
    pub died: bool,
    pub unmatched_closes: u64,
    pub skipped: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortfolioReport {
    pub agents: usize,
    pub ticks: u32,
    pub total_return_pct: f64,
    pub max_drawdown_pct: f64,
    pub sharpe_per_tick: f64,
    pub total_withdrawn: f64,
    pub deaths: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub run_id: Option<String>,
    pub agents: Vec<AgentReport>,
    pub portfolio: PortfolioReport,
}

fn analyze_agent(agent_id: &str, records: &[&EventRecord]) -> AgentReport {
    let mut ordered: Vec<&EventRecord> = records.to_vec();
    ordered.sort_by_key(|r| r.tick);

    let equity_ticks: Vec<(u32, f64)> =
        ordered.iter().filter_map(|r| (r.kind == "tick").then(|| num(&r.data, "equity")).flatten().map(|e| (r.tick, e))).collect();
    let splits: Vec<(u32, f64)> =
        ordered.iter().filter_map(|r| (r.kind == "split").then(|| num(&r.data, "withdrawn")).flatten().map(|w| (r.tick, w))).collect();

    let (recon, mask) = reconstruct(agent_id, &ordered);

    let curve = value_curve(&equity_ticks, &splits);
    let values: Vec<f64> = curve.iter().map(|(_, v)| *v).collect();

    let trade_stats = TradeStats::from_trades(&recon.trades);

    let last_final = ordered.iter().rev().find(|r| r.kind == "final_summary");
    let final_equity = last_final
        .and_then(|r| num(&r.data, "equity"))
        .or_else(|| equity_ticks.last().map(|(_, e)| *e))
        .unwrap_or(0.0);
    let total_withdrawn = last_final
        .and_then(|r| num(&r.data, "withdrawn"))
        .unwrap_or_else(|| splits.iter().map(|(_, w)| *w).sum());
    let initial = ordered
        .iter()
        .find_map(|r| match r.kind.as_str() {
            "snapshot" | "final_summary" => num(&r.data, "baseline"),
            _ => None,
        })
        .or_else(|| equity_ticks.first().map(|(_, e)| *e))
        .unwrap_or(0.0);
    let total_return_pct = if initial > 0.0 {
        (final_equity + total_withdrawn - initial) / initial * 100.0
    } else {
        0.0
    };

    let ticks = equity_ticks.len() as u32;
    let time_in_market_pct = if mask.is_empty() {
        0.0
    } else {
        mask.iter().filter(|(_, open)| *open).count() as f64 / mask.len() as f64 * 100.0
    };

    let died = ordered.iter().any(|r| {
        r.kind == "died"
            || (r.kind == "final_summary" && r.data.get("status").and_then(|s| s.as_str()) == Some("Dead"))
    });

    AgentReport {
        agent_id: agent_id.to_string(),
        ticks,
        time_in_market_pct,
        trade_stats,
        max_drawdown_pct: max_drawdown_pct(&values),
        sharpe_per_tick: sharpe_per_tick(&values),
        sortino_per_tick: sortino_per_tick(&values),
        final_equity,
        total_withdrawn,
        total_return_pct,
        splits: splits.len() as u32,
        died,
        unmatched_closes: recon.unmatched_closes,
        skipped: recon.skipped,
    }
}

/// Portfolio value curve: per-tick sum of each agent's latest known
/// total value (forward-filled; an agent counts from its first tick).
fn portfolio_curve(agents: &BTreeMap<String, Vec<(u32, f64)>>) -> Vec<(u32, f64)> {
    let mut ticks: Vec<u32> = agents.values().flat_map(|v| v.iter().map(|(t, _)| *t)).collect();
    ticks.sort_unstable();
    ticks.dedup();
    ticks
        .into_iter()
        .map(|t| {
            let sum: f64 = agents
                .values()
                .filter_map(|curve| {
                    let mut last = None;
                    for (ct, v) in curve {
                        if *ct <= t {
                            last = Some(*v);
                        } else {
                            break;
                        }
                    }
                    last
                })
                .sum();
            (t, sum)
        })
        .collect()
}

pub fn analyze(records: &[EventRecord]) -> Report {
    let mut by_agent: BTreeMap<String, Vec<&EventRecord>> = BTreeMap::new();
    for r in records {
        by_agent.entry(r.agent_id.clone()).or_default().push(r);
    }
    let agents: Vec<AgentReport> =
        by_agent.iter().map(|(id, recs)| analyze_agent(id, recs)).collect();

    // Rebuild per-agent value curves for the portfolio merge.
    let mut curves: BTreeMap<String, Vec<(u32, f64)>> = BTreeMap::new();
    for (id, recs) in &by_agent {
        let mut ordered: Vec<&&EventRecord> = recs.iter().collect();
        ordered.sort_by_key(|r| r.tick);
        let equity: Vec<(u32, f64)> = ordered
            .iter()
            .filter_map(|r| (r.kind == "tick").then(|| num(&r.data, "equity")).flatten().map(|e| (r.tick, e)))
            .collect();
        let splits: Vec<(u32, f64)> = ordered
            .iter()
            .filter_map(|r| (r.kind == "split").then(|| num(&r.data, "withdrawn")).flatten().map(|w| (r.tick, w)))
            .collect();
        curves.insert(id.clone(), value_curve(&equity, &splits));
    }
    let merged = portfolio_curve(&curves);
    let pvalues: Vec<f64> = merged.iter().map(|(_, v)| *v).collect();
    let initial: f64 = curves.values().filter_map(|c| c.first().map(|(_, v)| *v)).sum();
    // Curves already include cumulative withdrawals (value_curve),
    // so the last points are directly comparable to the first.
    let final_total: f64 = curves.values().filter_map(|c| c.last().map(|(_, v)| *v)).sum();
    let total_return_pct = if initial > 0.0 { (final_total - initial) / initial * 100.0 } else { 0.0 };

    Report {
        run_id: records.first().map(|r| r.run_id.clone()),
        portfolio: PortfolioReport {
            agents: agents.len(),
            ticks: merged.len() as u32,
            total_return_pct,
            max_drawdown_pct: max_drawdown_pct(&pvalues),
            sharpe_per_tick: sharpe_per_tick(&pvalues),
            total_withdrawn: agents.iter().map(|a| a.total_withdrawn).sum(),
            deaths: agents.iter().filter(|a| a.died).count(),
        },
        agents,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rec(agent: &str, tick: u32, kind: &str, data: serde_json::Value) -> EventRecord {
        EventRecord {
            ts: Utc::now(),
            run_id: "r".to_string(),
            agent_id: agent.to_string(),
            tick,
            kind: kind.to_string(),
            data,
        }
    }

    fn tick(agent: &str, t: u32, equity: f64) -> EventRecord {
        rec(agent, t, "tick", serde_json::json!({ "equity": equity }))
    }

    #[test]
    fn empty_log_gives_empty_report() {
        let r = analyze(&[]);
        assert!(r.agents.is_empty());
        assert_eq!(r.portfolio.agents, 0);
        assert_eq!(r.run_id, None);
    }

    #[test]
    fn return_accounts_withdrawals_not_as_loss() {
        // Flat $100 ticks, then a $100 split halves equity: return must
        // be ~0%, not -50%.
        let log = vec![
            tick("a", 1, 100.0),
            tick("a", 2, 200.0),
            rec("a", 2, "split", serde_json::json!({ "withdrawn": 100.0, "new_baseline": 100.0 })),
            tick("a", 3, 100.0),
            rec("a", 3, "snapshot", serde_json::json!({
                "balance": 100.0, "open_units": 0.0, "entry_price": null,
                "baseline": 100.0, "withdrawn": 100.0,
            })),
        ];
        let r = analyze(&log);
        assert_eq!(r.agents.len(), 1);
        let a = &r.agents[0];
        assert!((a.total_return_pct - 100.0).abs() < 1e-6, "return={}", a.total_return_pct);
        assert_eq!(a.splits, 1);
    }
}
