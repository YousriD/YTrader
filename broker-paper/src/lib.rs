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
        }
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

        let notional = order.units * fill_price;
        if order.side == Side::Buy && notional > self.balance {
            return Err(BrokerError::InsufficientBalance);
        }

        // Realize P&L against existing position, then update position.
        let signed_units = match order.side {
            Side::Buy => order.units,
            Side::Sell => -order.units,
        };

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
        }

        self.open_units += signed_units;
        if self.open_units == 0.0 {
            self.avg_entry_price = 0.0;
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
}
