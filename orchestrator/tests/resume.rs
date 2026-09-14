//! Automated restart test (P1-1 remainder): trade N ticks, persist state
//! through the SAME constructors the orchestrator logger uses, then
//! rebuild from the log exactly like `--resume` does and assert the
//! reconstructed state matches. Catches on-disk shape drift between
//! writer and reader by construction.

use async_trait::async_trait;
use broker_paper::PaperBroker;
use chrono::Utc;
use persistence::{final_record, latest_run_file, load_snapshots, snapshot_record, EventLog, Snapshot};
use trading_core::{Candle, MarketContext, Order, Side, Strategy};

struct BuyThenClose {
    units: f64,
    step: u8,
}

#[async_trait]
impl Strategy for BuyThenClose {
    fn name(&self) -> &str {
        "buy-then-close"
    }
    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        self.step += 1;
        match self.step {
            1 => Some(Order { symbol: ctx.symbol.to_string(), side: Side::Buy, units: self.units }),
            2 => Some(Order {
                symbol: ctx.symbol.to_string(),
                side: Side::Sell,
                units: ctx.account.open_units.abs(),
            }),
            _ => None,
        }
    }
}

struct Hold;

#[async_trait]
impl Strategy for Hold {
    fn name(&self) -> &str {
        "hold"
    }
    async fn decide(&mut self, _ctx: &MarketContext<'_>) -> Option<Order> {
        None
    }
}

fn candle(price: f64) -> Candle {
    Candle { time: Utc::now(), open: price, high: price, low: price, close: price }
}

fn snap_of(agent: &agent_runtime::Agent) -> Snapshot {
    let st = agent.account_state();
    Snapshot {
        balance: st.balance,
        open_units: st.open_units,
        entry_price: st.entry_price,
        baseline: agent.baseline(),
        withdrawn: agent.total_withdrawn,
    }
}

#[tokio::test]
async fn restart_rebuilds_agent_state_from_log() {
    let dir = std::env::temp_dir().join("tradery-resume-e2e");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Run 1: buy 50 @1.0, close @3.1 (realizes ~+105, split fires same tick).
    let mut agent = agent_runtime::Agent::new(
        "algo",
        "EUR_USD",
        Box::new(BuyThenClose { units: 50.0, step: 0 }),
        Box::new(PaperBroker::new(100.0)),
        100.0,
    );
    agent.on_tick(candle(1.0)).await;
    agent.on_tick(candle(3.1)).await;
    let before = snap_of(&agent);
    assert!(before.withdrawn > 90.0, "split should have fired: {before:?}");
    let equity_before = agent.account_state().equity;

    // Persist through the shared constructors (same shape as the logger).
    let log = EventLog::open(dir.join("run-e2e.jsonl")).unwrap();
    log.append(&snapshot_record("run1", "algo", 2, &before)).unwrap();
    log.append(&final_record("run1", "algo", "Alive", equity_before, &before)).unwrap();

    // Restart: newest file -> fold -> restore, mirroring main().
    let latest = latest_run_file(&dir).unwrap();
    assert!(latest.ends_with("run-e2e.jsonl"));
    let map = load_snapshots(&latest).unwrap();
    let snap = map.get("algo").expect("snapshot must survive restart");
    let broker =
        PaperBroker::restore(snap.balance, snap.open_units, snap.entry_price.unwrap_or(0.0)).unwrap();
    // Fresh strategy on restart (memory rebuilds); Hold keeps the tick clean.
    let mut restarted =
        agent_runtime::Agent::restore("algo", "EUR_USD", Box::new(Hold), Box::new(broker), snap.baseline, snap.withdrawn);

    let st = restarted.account_state();
    assert!((st.balance - before.balance).abs() < 1e-9);
    assert_eq!(st.open_units, before.open_units);
    assert_eq!(st.entry_price, before.entry_price);
    // Money is f64 and crossed JSON: compare with tolerance, never ==.
    assert!((restarted.baseline() - before.baseline).abs() < 1e-9);
    assert!((restarted.total_withdrawn - before.withdrawn).abs() < 1e-9);

    // Flat tick at the last price: identical equity, no spurious split.
    let events = restarted.on_tick(candle(3.1)).await;
    assert!(
        events.iter().all(|e| !matches!(e, agent_runtime::AgentEvent::Split { .. })),
        "spurious split after restart: {events:?}"
    );
    assert!((restarted.account_state().equity - equity_before).abs() < 1e-6);

    let _ = std::fs::remove_dir_all(&dir);
}
