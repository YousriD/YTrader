//! Per-agent metrics over reconstructed state (P2-2).
//!
//! All equity-based figures use TOTAL account value (equity +
//! cumulative withdrawn): a split halves equity without losing a cent,
//! so raw equity curves would fake a 50% drawdown at every split.
//! Per-tick Sharpe/Sortino are raw (no annualization — ticks have no
//! wall-clock meaning); compare runs relatively, not absolutely.

use serde::Serialize;

use crate::reconstruct::ClosedTrade;

/// Worst peak-to-trough percentage on a value series. Empty/singleton → 0.
pub fn max_drawdown_pct(values: &[f64]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    let mut worst = 0.0f64;
    for &v in values {
        if v > peak {
            peak = v;
        }
        if peak > 0.0 {
            worst = worst.max((peak - v) / peak * 100.0);
        }
    }
    worst
}

/// Mean / std of simple relative returns between consecutive values.
fn returns(values: &[f64]) -> Vec<f64> {
    values
        .windows(2)
        .filter_map(|w| if w[0] > 0.0 { Some((w[1] - w[0]) / w[0]) } else { None })
        .collect()
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

fn std(xs: &[f64], m: f64) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64).sqrt()
}

/// Per-tick Sharpe (risk-free 0). Zero-variance → 0.0 by definition.
pub fn sharpe_per_tick(values: &[f64]) -> f64 {
    let r = returns(values);
    let m = mean(&r);
    let s = std(&r, m);
    if s == 0.0 {
        0.0
    } else {
        m / s
    }
}

/// Per-tick Sortino (downside deviation). No downside → None (not
/// zero — a loss-free run must not read as risk-neutral).
pub fn sortino_per_tick(values: &[f64]) -> Option<f64> {
    let r = returns(values);
    let m = mean(&r);
    let down: Vec<f64> = r.iter().cloned().filter(|x| *x < 0.0).collect();
    if down.is_empty() {
        return None;
    }
    let ds = std(&down, m);
    if ds == 0.0 {
        None
    } else {
        Some(m / ds)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TradeStats {
    pub trades: usize,
    pub wins: usize,
    pub win_rate_pct: f64,
    /// Gross profit / gross loss. None when nothing was ever lost
    /// (covers zero-trade and all-win runs — ratio undefined, not zero).
    pub profit_factor: Option<f64>,
    pub avg_win: f64,
    pub avg_loss: f64,
    /// Mean P&L per closed trade.
    pub expectancy: f64,
}

impl TradeStats {
    pub fn from_trades(trades: &[ClosedTrade]) -> Self {
        let wins: Vec<f64> = trades.iter().map(|t| t.pnl).filter(|p| *p > 0.0).collect();
        let losses: Vec<f64> = trades.iter().map(|t| t.pnl).filter(|p| *p <= 0.0).collect();
        let gross_profit: f64 = wins.iter().copied().sum();
        let gross_loss: f64 = losses.iter().copied().map(|l| l.abs()).sum();
        let n = trades.len();
        TradeStats {
            trades: n,
            wins: wins.len(),
            win_rate_pct: if n > 0 { wins.len() as f64 / n as f64 * 100.0 } else { 0.0 },
            profit_factor: if gross_loss > 0.0 { Some(gross_profit / gross_loss) } else { None },
            avg_win: if wins.is_empty() { 0.0 } else { gross_profit / wins.len() as f64 },
            avg_loss: if losses.is_empty() { 0.0 } else { gross_loss / losses.len() as f64 },
            expectancy: if n > 0 {
                (gross_profit - gross_loss) / n as f64
            } else {
                0.0
            },
        }
    }
}

/// Total-value curve: tick equity + cumulative withdrawn at that tick.
/// Withdrawn timeline comes from split amounts in tick order.
pub fn value_curve(equity_ticks: &[(u32, f64)], splits: &[(u32, f64)]) -> Vec<(u32, f64)> {
    let mut out = Vec::with_capacity(equity_ticks.len());
    let mut cum = 0.0;
    let mut si = 0;
    let mut ordered: Vec<(u32, f64)> = splits.to_vec();
    ordered.sort_by_key(|(t, _)| *t);
    for &(tick, equity) in equity_ticks {
        while si < ordered.len() && ordered[si].0 <= tick {
            cum += ordered[si].1;
            si += 1;
        }
        out.push((tick, equity + cum));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconstruct::ClosedTrade;
    use trading_core::Side;

    fn trade(pnl: f64) -> ClosedTrade {
        ClosedTrade {
            agent_id: "a".to_string(),
            open_tick: 0,
            close_tick: 1,
            direction: Side::Buy,
            units: 1.0,
            open_price: 1.0,
            close_price: 1.0,
            pnl,
        }
    }

    #[test]
    fn drawdown_finds_worst_peak_trough() {
        assert_eq!(max_drawdown_pct(&[]), 0.0);
        assert_eq!(max_drawdown_pct(&[100.0]), 0.0);
        // Split-adjusted thinking: 100 -> 200 -> 150 -> 250 => 25%.
        assert!((max_drawdown_pct(&[100.0, 200.0, 150.0, 250.0]) - 25.0).abs() < 1e-9);
        assert_eq!(max_drawdown_pct(&[100.0, 110.0, 120.0]), 0.0);
    }

    #[test]
    fn sharpe_zero_variance_is_zero_not_nan() {
        assert_eq!(sharpe_per_tick(&[100.0, 100.0, 100.0]), 0.0);
        assert_eq!(sharpe_per_tick(&[100.0]), 0.0);
        // Steady climb => positive.
        assert!(sharpe_per_tick(&[100.0, 101.0, 102.0, 103.0]) > 0.0);
    }

    #[test]
    fn sortino_none_without_downside() {
        assert_eq!(sortino_per_tick(&[100.0, 101.0, 102.0]), None);
        // A single down-tick leaves sample downside-deviation undefined.
        assert_eq!(sortino_per_tick(&[100.0, 110.0, 100.0]), None);
        let mixed = [100.0, 110.0, 100.0, 110.0, 95.0, 105.0];
        assert!(sortino_per_tick(&mixed).is_some());
    }

    #[test]
    fn trade_stats_win_loss_and_undefined_pf() {
        let s = TradeStats::from_trades(&[trade(10.0), trade(-4.0), trade(6.0)]);
        assert!((s.win_rate_pct - 200.0 / 3.0).abs() < 1e-9, "{}", s.win_rate_pct);
        assert!((s.profit_factor.unwrap() - 4.0).abs() < 1e-9);
        assert!((s.expectancy - 4.0).abs() < 1e-9);
        let empty = TradeStats::from_trades(&[]);
        assert_eq!(empty.profit_factor, None);
        assert_eq!(empty.win_rate_pct, 0.0);
        let all_win = TradeStats::from_trades(&[trade(5.0)]);
        assert_eq!(all_win.profit_factor, None); // undefined, not zero
    }

    #[test]
    fn value_curve_adds_withdrawals_at_their_tick() {
        let curve = value_curve(&[(1, 100.0), (2, 100.0), (3, 60.0)], &[(2, 40.0)]);
        assert_eq!(curve, vec![(1, 100.0), (2, 140.0), (3, 100.0)]);
        // No phantom drawdown across the split: 100 -> 140 -> 100.
        let values: Vec<f64> = curve.iter().map(|(_, v)| *v).collect();
        assert!((max_drawdown_pct(&values) - 40.0 / 140.0 * 100.0).abs() < 1e-9);
    }
}
