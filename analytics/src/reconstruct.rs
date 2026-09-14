//! Trade reconstruction from the event log (P2-2).
//!
//! Mirrors the broker's average-cost matching (open / add / partial
//! close / flip) over each agent's fills in tick order. Snapshot records
//! re-seed the mirror, so positions opened before the log (or before a
//! crash window) still reconcile: snapshots ARE the truth at their tick.
//! Fills that arrive with no open position to close against (pre-log
//! opens on resumed runs) are counted as `unmatched_closes`, never
//! invented. Old logs without fill prices are skipped via `skipped`.

use persistence::EventRecord;
use trading_core::Side;

/// One closed position segment, gross of commission (commissions are not
/// logged per fill — they show up in equity-based metrics instead).
#[derive(Debug, Clone, PartialEq)]
pub struct ClosedTrade {
    pub agent_id: String,
    pub open_tick: u32,
    pub close_tick: u32,
    /// Side of the closed position: Buy = was long, Sell = was short.
    pub direction: Side,
    pub units: f64,
    pub open_price: f64,
    pub close_price: f64,
    pub pnl: f64,
}

#[derive(Debug, Default)]
struct Mirror {
    units: f64, // signed: +long / -short
    avg: f64,
    open_tick: u32,
}

#[derive(Debug, Default)]
pub struct Reconstruction {
    pub trades: Vec<ClosedTrade>,
    pub unmatched_closes: u64,
    pub skipped: u64,
}

fn num(data: &serde_json::Value, key: &str) -> Option<f64> {
    data.get(key)?.as_f64()
}

fn side_of(data: &serde_json::Value) -> Option<Side> {
    match data.get("side")?.as_str()? {
        "Buy" => Some(Side::Buy),
        "Sell" => Some(Side::Sell),
        _ => None,
    }
}

fn apply_fill(
    out: &mut Reconstruction,
    mirror: &mut Mirror,
    agent_id: &str,
    tick: u32,
    side: Side,
    units: f64,
    price: f64,
) {
    if !(units.is_finite() && units > 0.0 && price.is_finite() && price > 0.0) {
        out.skipped += 1;
        return;
    }
    let signed = match side {
        Side::Buy => units,
        Side::Sell => -units,
    };
    if mirror.units == 0.0 {
        mirror.units = signed;
        mirror.avg = price;
        mirror.open_tick = tick;
    } else if mirror.units.signum() == signed.signum() {
        let total = mirror.units + signed;
        mirror.avg = (mirror.avg * mirror.units + price * signed) / total;
        mirror.units = total;
    } else {
        let closing = signed.abs().min(mirror.units.abs());
        let per_unit = if mirror.units > 0.0 { price - mirror.avg } else { mirror.avg - price };
        out.trades.push(ClosedTrade {
            agent_id: agent_id.to_string(),
            open_tick: mirror.open_tick,
            close_tick: tick,
            direction: if mirror.units > 0.0 { Side::Buy } else { Side::Sell },
            units: closing,
            open_price: mirror.avg,
            close_price: price,
            pnl: per_unit * closing,
        });
        let old_sign = mirror.units.signum();
        mirror.units += signed;
        if mirror.units == 0.0 {
            mirror.avg = 0.0;
        } else if mirror.units.signum() != old_sign {
            // Flipped sides: leftover is a NEW position at this fill.
            // Partial closes keep the old average (broker-identical).
            mirror.avg = price;
            mirror.open_tick = tick;
        }
    }
}

/// Fold one agent's records (tick order) into closed trades + position.
/// Also returns the open/closed tick mask used for time-in-market.
pub fn reconstruct(agent_id: &str, records: &[&EventRecord]) -> (Reconstruction, Vec<(u32, bool)>) {
    let mut out = Reconstruction::default();
    let mut mirror = Mirror::default();
    let mut position_at_tick = Vec::new();
    for r in records {
        match r.kind.as_str() {
            "snapshot" | "final_summary" => {
                // Re-seed from truth (only when the record carries state —
                // old finals without balance keys are ignored, not fatal).
                if let (Some(balance), Some(units)) =
                    (num(&r.data, "balance"), num(&r.data, "open_units"))
                {
                    let _ = balance;
                    let entry = r.data.get("entry_price").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    if units == 0.0 {
                        mirror.units = 0.0;
                        mirror.avg = 0.0;
                    } else if entry > 0.0 {
                        mirror.units = units;
                        mirror.avg = entry;
                        // open_tick unknown for pre-log positions: keep
                        // existing if already tracking, else this tick.
                        if mirror.open_tick == 0 {
                            mirror.open_tick = r.tick;
                        }
                    }
                }
            }
            "order_placed" => {
                match (side_of(&r.data), num(&r.data, "units"), num(&r.data, "price")) {
                    (Some(side), Some(units), Some(price)) => {
                        apply_fill(&mut out, &mut mirror, agent_id, r.tick, side, units, price)
                    }
                    _ => out.skipped += 1, // pre-price-enrichment log
                }
            }
            "stop_loss_hit" | "take_profit_hit" => {
                match (num(&r.data, "price"), num(&r.data, "closed_units")) {
                    (Some(price), Some(closed)) if mirror.units != 0.0 => {
                        let side = if mirror.units > 0.0 { Side::Sell } else { Side::Buy };
                        let open = mirror.units.abs();
                        apply_fill(&mut out, &mut mirror, agent_id, r.tick, side, closed.min(open), price);
                    }
                    _ => out.unmatched_closes += 1,
                }
            }
            "tick" => {
                position_at_tick.push((r.tick, mirror.units != 0.0));
            }
            _ => {}
        }
    }
    (out, position_at_tick)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn rec(agent: &str, tick: u32, kind: &str, data: serde_json::Value) -> EventRecord {
        EventRecord {
            ts: Utc::now(),
            run_id: "t".to_string(),
            agent_id: agent.to_string(),
            tick,
            kind: kind.to_string(),
            data,
        }
    }

    fn fill(tick: u32, side: &str, units: f64, price: f64) -> EventRecord {
        rec("a", tick, "order_placed", serde_json::json!({
            "symbol": "EUR_USD", "side": side, "units": units, "price": price,
        }))
    }

    fn refs(records: &[EventRecord]) -> Vec<&EventRecord> {
        records.iter().collect()
    }

    #[test]
    fn long_win_and_loss() {
        let log = vec![
            fill(1, "Buy", 10.0, 1.0),
            fill(2, "Sell", 10.0, 1.5), // +5
            fill(3, "Buy", 10.0, 2.0),
            fill(4, "Sell", 10.0, 1.0), // -10
        ];
        let (r, _) = reconstruct("a", &refs(&log));
        assert_eq!(r.trades.len(), 2);
        assert!((r.trades[0].pnl - 5.0).abs() < 1e-9);
        assert!((r.trades[1].pnl + 10.0).abs() < 1e-9);
        assert_eq!(r.trades[0].direction, Side::Buy);
    }

    #[test]
    fn pyramid_add_then_partial_close() {
        let log = vec![
            fill(1, "Buy", 10.0, 1.0),
            fill(2, "Buy", 10.0, 3.0), // avg 2.0
            fill(3, "Sell", 5.0, 4.0), // +10 on 5
        ];
        let (r, _) = reconstruct("a", &refs(&log));
        assert_eq!(r.trades.len(), 1);
        assert!((r.trades[0].pnl - 10.0).abs() < 1e-9);
        assert!((r.trades[0].open_price - 2.0).abs() < 1e-9);
    }

    #[test]
    fn flip_books_close_and_reprices_leftover() {
        let log = vec![
            fill(1, "Buy", 10.0, 1.0),
            fill(2, "Sell", 25.0, 2.0), // close 10 (+10), short 15 @2.0
            fill(3, "Buy", 15.0, 1.0),  // cover: +15
        ];
        let (r, _) = reconstruct("a", &refs(&log));
        assert_eq!(r.trades.len(), 2);
        assert!((r.trades[0].pnl - 10.0).abs() < 1e-9);
        assert!((r.trades[1].pnl - 15.0).abs() < 1e-9);
        assert_eq!(r.trades[1].direction, Side::Sell);
    }

    #[test]
    fn snapshot_seeds_pre_log_position() {
        let log = vec![
            rec("a", 5, "snapshot", serde_json::json!({
                "balance": 100.0, "open_units": 10.0, "entry_price": 1.0,
                "baseline": 100.0, "withdrawn": 0.0,
            })),
            rec("a", 6, "stop_loss_hit", serde_json::json!({
                "price": 2.0, "move_pct": 1.0, "closed_units": 10.0,
            })),
        ];
        let (r, _) = reconstruct("a", &refs(&log));
        assert_eq!(r.trades.len(), 1);
        assert!((r.trades[0].pnl - 10.0).abs() < 1e-9);
        assert_eq!(r.unmatched_closes, 0);
    }

    #[test]
    fn orphan_close_counted_not_invented() {
        let log = vec![rec("a", 6, "take_profit_hit", serde_json::json!({
            "price": 2.0, "move_pct": 0.02, "closed_units": 10.0,
        }))];
        let (r, _) = reconstruct("a", &refs(&log));
        assert!(r.trades.is_empty());
        assert_eq!(r.unmatched_closes, 1);
    }

    #[test]
    fn old_logs_without_prices_skip_cleanly() {
        let log = vec![rec("a", 1, "order_placed", serde_json::json!({
            "symbol": "EUR_USD", "side": "Buy", "units": 10.0,
        }))];
        let (r, _) = reconstruct("a", &refs(&log));
        assert!(r.trades.is_empty());
        assert_eq!(r.skipped, 1);
    }

    /// Property check: the mirror must reproduce the broker's own
    /// realized P&L to the cent when fed identical fills (same shapes
    /// `persist_event` writes). Guards against mirror/broker drift.
    #[tokio::test]
    async fn mirror_matches_broker_balance() {
        use broker_paper::PaperBroker;
        use trading_core::{Broker, Order};
        let mut broker = PaperBroker::new(10_000.0);
        let ops = [
            (Side::Buy, 10.0, 1.0),
            (Side::Buy, 10.0, 2.0),
            (Side::Sell, 5.0, 3.0),
            (Side::Sell, 15.0, 2.5),
            (Side::Sell, 8.0, 2.0),
            (Side::Buy, 8.0, 1.0),
        ];
        let mut log = Vec::new();
        for (i, (side, units, mark)) in ops.iter().enumerate() {
            broker.mark_price(*mark);
            let fill = broker
                .place_order(Order { symbol: "EUR_USD".to_string(), side: *side, units: *units })
                .await
                .unwrap();
            log.push(rec("a", i as u32, "order_placed", serde_json::json!({
                "symbol": "EUR_USD", "side": format!("{side:?}"),
                "units": units, "price": fill.price,
            })));
        }
        assert_eq!(broker.account_state().open_units, 0.0);
        let (r, _) = reconstruct("a", &refs(&log));
        let realized: f64 = r.trades.iter().map(|t| t.pnl).sum();
        let delta = broker.account_state().balance - 10_000.0;
        assert!((realized - delta).abs() < 1e-6, "mirror={realized} broker={delta}");
    }
}
