use async_trait::async_trait;
use chrono::Utc;
use trading_core::{AccountState, Broker, BrokerError, Fill, Order, Side};

/// Simulates order fills at the last known price plus a small fixed spread,
/// so paper-trading P&L behaves like a real (if idealized) account.
/// Swap this out for a `broker-oanda` crate implementing the same `Broker`
/// trait to go live — nothing above this layer changes.
pub struct PaperBroker {
    balance: f64,
    open_units: f64, // positive = long, negative = short
    avg_entry_price: f64,
    last_price: Option<f64>,
    spread_pips: f64,
    pip_value: f64, // simplistic: price move per "pip" for the symbol
    max_leverage: f64, // 1.0 = spot-parity: total exposure may not exceed balance
    // P2-1 venue economics: what the venue's rulebook charges/requires.
    min_units: f64,          // smallest order accepted (default 1.0)
    min_notional: f64,       // smallest notional accepted; 0.0 = no floor
    commission_per_unit: f64, // cash deducted per filled unit, every fill
}

impl PaperBroker {
    pub fn new(starting_balance: f64) -> Self {
        Self {
            balance: starting_balance,
            open_units: 0.0,
            avg_entry_price: 0.0,
            last_price: None,
            spread_pips: 1.2,
            pip_value: 0.0001,
            max_leverage: 1.0,
            min_units: 1.0,
            min_notional: 0.0,
            commission_per_unit: 0.0,
        }
    }

    pub fn with_max_leverage(mut self, leverage: f64) -> Self {
        self.max_leverage = leverage.max(1.0);
        self
    }

    /// Venue rulebook overrides (P2-1). Each validates its input so a
    /// misconfigured venue fails fast at startup, not mid-run.
    pub fn with_min_units(mut self, min_units: f64) -> Self {
        self.min_units = if min_units.is_finite() { min_units.max(1.0) } else { 1.0 };
        self
    }

    pub fn with_min_notional(mut self, min_notional: f64) -> Self {
        self.min_notional = if min_notional.is_finite() { min_notional.max(0.0) } else { 0.0 };
        self
    }

    pub fn with_commission_per_unit(mut self, rate: f64) -> Self {
        self.commission_per_unit = if rate.is_finite() { rate.max(0.0) } else { 0.0 };
        self
    }

    pub fn with_spread_pips(mut self, pips: f64) -> Self {
        self.spread_pips = if pips.is_finite() { pips.max(0.0) } else { self.spread_pips };
        self
    }

    /// Rebuild a broker from a persisted snapshot (see P1-1). Flat state
    /// must be exactly `(open_units == 0, avg == 0)` or vice versa —
    /// anything else is a corrupt snapshot and is rejected.
    pub fn restore(balance: f64, open_units: f64, avg_entry_price: f64) -> Result<Self, BrokerError> {
        if !balance.is_finite() || !open_units.is_finite() || !avg_entry_price.is_finite() {
            return Err(BrokerError::Other("corrupt snapshot: non-finite value".to_string()));
        }
        if (open_units == 0.0) != (avg_entry_price == 0.0) {
            return Err(BrokerError::Other("corrupt snapshot: position/entry mismatch".to_string()));
        }
        let mut b = Self::new(balance);
        b.open_units = open_units;
        b.avg_entry_price = avg_entry_price;
        Ok(b)
    }

    fn spread(&self) -> f64 {
        self.spread_pips * self.pip_value
    }
}

#[async_trait]
impl Broker for PaperBroker {
    async fn place_order(&mut self, order: Order) -> Result<Fill, BrokerError> {
        let mid = self.last_price.ok_or(BrokerError::NoPrice)?;
        let half_spread = self.spread() / 2.0;
        let fill_price = match order.side {
            Side::Buy => mid + half_spread,
            Side::Sell => mid - half_spread,
        };

        if !order.units.is_finite() || order.units <= 0.0 {
            return Err(BrokerError::Other("order units must be positive".to_string()));
        }

        // P2-1 venue minimums: rejected before margin so a too-small
        // order reads as a rulebook rejection, not an affordability one.
        if order.units < self.min_units {
            return Err(BrokerError::Other(format!(
                "below venue minimum of {} units",
                self.min_units
            )));
        }
        let notional = order.units * fill_price;
        if self.min_notional > 0.0 && notional < self.min_notional {
            return Err(BrokerError::Other(format!(
                "below venue minimum notional of ${:.2}",
                self.min_notional
            )));
        }

        // Margin: only orders that INCREASE absolute exposure need cover.
        // Closes / partial closes (exposure shrinks) always pass so SL/TP
        // can always get out. New exposure may not exceed balance*leverage.
        let signed_units = match order.side {
            Side::Buy => order.units,
            Side::Sell => -order.units,
        };
        let new_open = self.open_units + signed_units;
        if new_open.abs() > self.open_units.abs()
            && new_open.abs() * fill_price > self.balance * self.max_leverage
        {
            return Err(BrokerError::InsufficientBalance);
        }

        // Realize P&L against existing position, then update position.
        if self.open_units == 0.0 {
            self.avg_entry_price = fill_price;
        } else if self.open_units.signum() == signed_units.signum() {
            // Adding to position: weighted average entry.
            let total = self.open_units + signed_units;
            self.avg_entry_price =
                (self.avg_entry_price * self.open_units + fill_price * signed_units) / total;
        } else {
            // Reducing or flipping: realize P&L on the closed portion.
            let closing_units = signed_units.abs().min(self.open_units.abs());
            let pnl_per_unit = if self.open_units > 0.0 {
                fill_price - self.avg_entry_price
            } else {
                self.avg_entry_price - fill_price
            };
            self.balance += pnl_per_unit * closing_units;
            if new_open != 0.0 && new_open.signum() != self.open_units.signum() {
                // Flipped sides: leftover is a NEW position at this fill.
                self.avg_entry_price = fill_price;
            }
            // Partial close without flip: keep old average (correct).
        }

        self.open_units += signed_units;
        if self.open_units == 0.0 {
            self.avg_entry_price = 0.0;
        }

        // P2-1 commission on every fill (opens AND closes, including
        // forced SL/TP exits). Costs can take balance negative — the
        // agent's die-at-zero check catches that next tick.
        if self.commission_per_unit > 0.0 {
            self.balance -= order.units * self.commission_per_unit;
        }

        Ok(Fill {
            order,
            price: fill_price,
            time: Utc::now(),
        })
    }

    fn account_state(&self) -> AccountState {
        let unrealized = match self.last_price {
            Some(p) if self.open_units != 0.0 => {
                let pnl_per_unit = if self.open_units > 0.0 {
                    p - self.avg_entry_price
                } else {
                    self.avg_entry_price - p
                };
                pnl_per_unit * self.open_units.abs()
            }
            _ => 0.0,
        };
        AccountState {
            balance: self.balance,
            equity: self.balance + unrealized,
            open_units: self.open_units,
            entry_price: if self.open_units != 0.0 {
                Some(self.avg_entry_price)
            } else {
                None
            },
        }
    }

    fn last_price(&self) -> Option<f64> {
        self.last_price
    }

    fn mark_price(&mut self, price: f64) {
        self.last_price = Some(price);
    }

    fn withdraw(&mut self, amount: f64) -> Result<(), BrokerError> {
        if !amount.is_finite() || amount <= 0.0 {
            return Err(BrokerError::Other("withdraw amount must be positive".to_string()));
        }
        if amount > self.balance {
            return Err(BrokerError::InsufficientBalance);
        }
        self.balance -= amount;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(side: Side, units: f64) -> Order {
        Order { symbol: "EUR_USD".to_string(), side, units }
    }

    #[tokio::test]
    async fn buy_rejects_when_notional_exceeds_balance() {
        let mut b = PaperBroker::new(50.0);
        b.mark_price(1.10);
        let err = b.place_order(order(Side::Buy, 1000.0)).await.unwrap_err();
        assert!(matches!(err, BrokerError::InsufficientBalance));
    }

    #[tokio::test]
    async fn sell_rejects_without_margin_cover() {
        let mut b = PaperBroker::new(50.0);
        b.mark_price(1.10);
        let err = b.place_order(order(Side::Sell, 1000.0)).await.unwrap_err();
        assert!(matches!(err, BrokerError::InsufficientBalance));
    }

    #[tokio::test]
    async fn rejects_non_positive_units() {
        let mut b = PaperBroker::new(1000.0);
        b.mark_price(1.0);
        assert!(b.place_order(order(Side::Buy, 0.0)).await.is_err());
        assert!(b.place_order(order(Side::Sell, -5.0)).await.is_err());
    }

    #[tokio::test]
    async fn add_to_position_uses_weighted_average() {
        let mut b = PaperBroker::new(10_000.0);
        b.mark_price(1.0);
        b.place_order(order(Side::Buy, 10.0)).await.unwrap();
        let first_fill = b.account_state().entry_price.unwrap();
        b.mark_price(3.0);
        b.place_order(order(Side::Buy, 10.0)).await.unwrap();
        let avg = b.account_state().entry_price.unwrap();
        let second_fill = 3.0 + b.spread() / 2.0;
        let expected = (first_fill * 10.0 + second_fill * 10.0) / 20.0;
        assert!((avg - expected).abs() < 1e-9, "avg={avg} expected={expected}");
    }

    #[tokio::test]
    async fn partial_close_keeps_average_entry() {
        let mut b = PaperBroker::new(10_000.0);
        b.mark_price(1.0);
        b.place_order(order(Side::Buy, 20.0)).await.unwrap();
        let entry = b.account_state().entry_price.unwrap();
        b.mark_price(2.0);
        b.place_order(order(Side::Sell, 5.0)).await.unwrap();
        let st = b.account_state();
        assert_eq!(st.open_units, 15.0);
        assert!((st.entry_price.unwrap() - entry).abs() < 1e-9);
    }

    #[tokio::test]
    async fn flip_resets_entry_to_new_fill() {
        let mut b = PaperBroker::new(10_000.0);
        b.mark_price(1.0);
        b.place_order(order(Side::Buy, 10.0)).await.unwrap();
        b.mark_price(2.0);
        b.place_order(order(Side::Sell, 25.0)).await.unwrap();
        let st = b.account_state();
        assert_eq!(st.open_units, -15.0);
        let expected_flip_fill = 2.0 - b.spread() / 2.0;
        assert!(
            (st.entry_price.unwrap() - expected_flip_fill).abs() < 1e-9,
            "entry={:?} expected={expected_flip_fill}",
            st.entry_price
        );
    }

    #[tokio::test]
    async fn closing_order_always_allowed_so_sltp_can_exit() {
        let mut b = PaperBroker::new(100.0);
        b.mark_price(1.0);
        b.place_order(order(Side::Buy, 10.0)).await.unwrap();
        b.withdraw(90.0).unwrap(); // drain cash; position still open
        b.mark_price(1.0);
        // Closing 10 (exposure shrinks 10 -> 0) must succeed despite tiny balance.
        b.place_order(order(Side::Sell, 10.0)).await.unwrap();
        assert_eq!(b.account_state().open_units, 0.0);
    }

    #[tokio::test]
    async fn withdraw_reduces_balance_and_rejects_overdraft() {
        let mut b = PaperBroker::new(100.0);
        b.mark_price(1.0);
        b.withdraw(40.0).unwrap();
        assert!((b.account_state().balance - 60.0).abs() < 1e-9);
        assert!(matches!(
            b.withdraw(100.0).unwrap_err(),
            BrokerError::InsufficientBalance
        ));
    }

    #[test]
    fn restore_rejects_position_entry_mismatch() {
        assert!(PaperBroker::restore(100.0, 0.0, 0.0).is_ok());
        assert!(PaperBroker::restore(100.0, 10.0, 1.1).is_ok());
        // Flat units with nonzero entry (or vice versa) is corrupt.
        assert!(PaperBroker::restore(100.0, 0.0, 1.1).is_err());
        assert!(PaperBroker::restore(100.0, 10.0, 0.0).is_err());
    }

    #[tokio::test]
    async fn rejects_below_venue_minimums() {
        let mut b = PaperBroker::new(10_000.0).with_min_units(100.0).with_min_notional(500.0);
        b.mark_price(1.0);
        assert!(b.place_order(order(Side::Buy, 10.0)).await.is_err()); // units
        assert!(b.place_order(order(Side::Buy, 100.0)).await.is_err()); // $100 notional
        b.place_order(order(Side::Buy, 600.0)).await.unwrap(); // $600 clears both
        assert_eq!(b.account_state().open_units, 600.0);
    }

    #[tokio::test]
    async fn commission_deducted_on_every_fill() {
        let mut b = PaperBroker::new(100.0).with_commission_per_unit(0.10);
        b.mark_price(1.0);
        b.place_order(order(Side::Buy, 10.0)).await.unwrap();
        assert!((b.account_state().balance - 99.0).abs() < 1e-9); // -$1.00
        b.mark_price(1.0);
        b.place_order(order(Side::Sell, 10.0)).await.unwrap(); // close also pays
        // 98.00 minus the round-trip spread cost (2 fills × 10 units × 0.00006).
        assert!((b.account_state().balance - 97.9988).abs() < 1e-6);
    }

    #[tokio::test]
    async fn spread_override_moves_fill_price() {
        let mut tight = PaperBroker::new(10_000.0);
        let mut wide = PaperBroker::new(10_000.0).with_spread_pips(10.0);
        tight.mark_price(1.0);
        wide.mark_price(1.0);
        let f_tight = tight.place_order(order(Side::Buy, 10.0)).await.unwrap();
        let f_wide = wide.place_order(order(Side::Buy, 10.0)).await.unwrap();
        assert!(f_wide.price > f_tight.price);
        assert!((f_wide.price - f_tight.price - (10.0 - 1.2) * 0.0001 / 2.0).abs() < 1e-9);
    }
}
