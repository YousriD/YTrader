use async_trait::async_trait;
use trading_core::{MarketContext, Order, Side, Strategy};

/// Classic fast/slow SMA crossover. Deliberately simple — it exists as
/// the algorithmic baseline agents get measured against, and as a
/// reference implementation for writing new Strategy plugins.
pub struct SmaCrossover {
    name: String,
    fast: usize,
    slow: usize,
    units: f64,
    was_fast_above: Option<bool>,
}

impl SmaCrossover {
    pub fn new(name: impl Into<String>, fast: usize, slow: usize, units: f64) -> Self {
        Self {
            name: name.into(),
            fast,
            slow,
            units,
            was_fast_above: None,
        }
    }

    fn sma(candles: &[f64], window: usize) -> Option<f64> {
        if candles.len() < window {
            return None;
        }
        let slice = &candles[candles.len() - window..];
        Some(slice.iter().sum::<f64>() / window as f64)
    }
}

#[async_trait]
impl Strategy for SmaCrossover {
    fn name(&self) -> &str {
        &self.name
    }

    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        let closes: Vec<f64> = ctx.history.iter().map(|c| c.close).collect();
        let fast_sma = Self::sma(&closes, self.fast)?;
        let slow_sma = Self::sma(&closes, self.slow)?;
        let fast_above = fast_sma > slow_sma;

        let signal = match self.was_fast_above {
            Some(prev) if prev != fast_above => Some(if fast_above { Side::Buy } else { Side::Sell }),
            _ => None,
        };
        self.was_fast_above = Some(fast_above);

        signal.map(|side| Order {
            symbol: ctx.symbol.to_string(),
            side,
            units: self.units,
        })
    }
}
