use async_trait::async_trait;
use trading_core::{Candle, MarketContext, Order, Side, Strategy};

use crate::inventory::{clamp_units, pyramid_blocked};

/// Donchian channel breakout: Buy when the current close exceeds the
/// highest high of the prior `channel` candles, Sell when it undercuts
/// the lowest low. Strict inequalities, so flat markets emit nothing.
/// Stateless apart from config — the channel is recomputed from history.
pub struct DonchianBreakout {
    name: String,
    channel: usize,
    units: f64,
    allow_pyramid: bool,
    max_units: f64,
}

impl DonchianBreakout {
    pub fn new(name: impl Into<String>, channel: usize, units: f64) -> Self {
        Self {
            name: name.into(),
            channel,
            units,
            allow_pyramid: false,
            max_units: units,
        }
    }

    pub fn with_inventory(mut self, allow_pyramid: bool, max_units: f64) -> Self {
        self.allow_pyramid = allow_pyramid;
        self.max_units = max_units;
        self
    }

    /// Breakout direction of the last candle vs the prior `channel`
    /// candles. Pure function of history — unit-testable.
    pub fn breakout(history: &[Candle], channel: usize) -> Option<Side> {
        if channel == 0 || history.len() < channel + 1 {
            return None;
        }
        let (prior, current) = history.split_at(history.len() - 1);
        let window = &prior[prior.len() - channel..];
        let highest = window.iter().map(|c| c.high).fold(f64::NEG_INFINITY, f64::max);
        let lowest = window.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);
        let close = current[0].close;
        if close > highest {
            Some(Side::Buy)
        } else if close < lowest {
            Some(Side::Sell)
        } else {
            None
        }
    }
}

#[async_trait]
impl Strategy for DonchianBreakout {
    fn name(&self) -> &str {
        &self.name
    }

    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        let side = Self::breakout(ctx.history, self.channel)?;
        if pyramid_blocked(ctx.account.open_units, self.allow_pyramid) {
            return None;
        }
        Some(Order {
            symbol: ctx.symbol.to_string(),
            side,
            units: clamp_units(self.units, self.max_units),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trading_core::{AccountState, NewsItem};
    use chrono::Utc;

    fn candle(o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle { time: Utc::now(), open: o, high: h, low: l, close: c }
    }

    fn flat(n: usize, price: f64) -> Vec<Candle> {
        (0..n).map(|_| candle(price, price, price, price)).collect()
    }

    fn ctx_for<'a>(history: &'a [Candle], news: &'a [NewsItem]) -> MarketContext<'a> {
        MarketContext {
            symbol: "EUR_USD",
            history,
            recent_news: news,
            account: AccountState { balance: 1000.0, equity: 1000.0, open_units: 0.0, entry_price: None },
        }
    }

    #[test]
    fn breakout_math_needs_channel_plus_one() {
        let h = flat(20, 1.0);
        assert_eq!(DonchianBreakout::breakout(&h, 20), None); // no current candle yet
        assert_eq!(DonchianBreakout::breakout(&h[..10], 20), None); // too short
    }

    #[tokio::test]
    async fn buys_upside_breakout_sells_downside() {
        let news: Vec<NewsItem> = Vec::new();
        let mut history = flat(20, 1.0);
        history.push(candle(1.0, 1.2, 1.0, 1.15)); // close > prior high 1.0
        let mut strat = DonchianBreakout::new("t", 20, 10.0);
        let order = strat.decide(&ctx_for(&history, &news)).await.expect("breakout Buy");
        assert_eq!(order.side, Side::Buy);

        let mut history = flat(20, 1.0);
        history.push(candle(1.0, 1.0, 0.8, 0.85)); // close < prior low 1.0
        let order = strat.decide(&ctx_for(&history, &news)).await.expect("breakdown Sell");
        assert_eq!(order.side, Side::Sell);
    }

    #[tokio::test]
    async fn flat_and_inside_channel_emit_nothing() {
        let news: Vec<NewsItem> = Vec::new();
        let mut strat = DonchianBreakout::new("t", 20, 10.0);
        let history = flat(25, 1.0);
        assert!(strat.decide(&ctx_for(&history, &news)).await.is_none());
        let mut history = flat(20, 1.0);
        history.push(candle(0.99, 1.0, 0.99, 1.0)); // close == high == low: no strict break
        assert!(strat.decide(&ctx_for(&history, &news)).await.is_none());
    }
}
