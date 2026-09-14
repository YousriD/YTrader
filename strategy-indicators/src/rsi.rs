use async_trait::async_trait;
use trading_core::{MarketContext, Order, Side, Strategy};

use crate::inventory::{clamp_units, pyramid_blocked};

/// Mean-reversion RSI (Wilder's smoothing, recomputed from history each
/// tick so there is no hidden state beyond the crossover flags).
///
/// Signals on regime EXIT, not entry: Buy when RSI crosses back up out
/// of oversold, Sell when it crosses back down out of overbought.
/// Entering oversold on a falling knife emits nothing by design.
pub struct Rsi {
    name: String,
    period: usize,
    overbought: f64,
    oversold: f64,
    units: f64,
    allow_pyramid: bool,
    max_units: f64,
    was_overbought: Option<bool>,
    was_oversold: Option<bool>,
}

impl Rsi {
    pub fn new(
        name: impl Into<String>,
        period: usize,
        overbought: f64,
        oversold: f64,
        units: f64,
    ) -> Self {
        Self {
            name: name.into(),
            period,
            overbought,
            oversold,
            units,
            allow_pyramid: false,
            max_units: units,
            was_overbought: None,
            was_oversold: None,
        }
    }

    pub fn with_inventory(mut self, allow_pyramid: bool, max_units: f64) -> Self {
        self.allow_pyramid = allow_pyramid;
        self.max_units = max_units;
        self
    }

    /// Wilder's RSI over closes. Needs `period + 1` closes minimum.
    /// Pure function of history — unit-testable without a strategy.
    pub fn rsi(closes: &[f64], period: usize) -> Option<f64> {
        let n = closes.len();
        if period == 0 || n < period + 1 {
            return None;
        }
        let p = period as f64;
        let (mut avg_gain, mut avg_loss) = (0.0, 0.0);
        for i in 0..period {
            let d = closes[i + 1] - closes[i];
            if d > 0.0 {
                avg_gain += d;
            } else {
                avg_loss -= d;
            }
        }
        avg_gain /= p;
        avg_loss /= p;
        for i in period..(n - 1) {
            let d = closes[i + 1] - closes[i];
            let (g, l) = if d > 0.0 { (d, 0.0) } else { (0.0, -d) };
            avg_gain = (avg_gain * (p - 1.0) + g) / p;
            avg_loss = (avg_loss * (p - 1.0) + l) / p;
        }
        if avg_loss == 0.0 {
            return Some(100.0);
        }
        let rs = avg_gain / avg_loss;
        Some(100.0 - 100.0 / (1.0 + rs))
    }
}

#[async_trait]
impl Strategy for Rsi {
    fn name(&self) -> &str {
        &self.name
    }

    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        let closes: Vec<f64> = ctx.history.iter().map(|c| c.close).collect();
        let value = Self::rsi(&closes, self.period)?;
        let over = value >= self.overbought;
        let under = value <= self.oversold;

        let signal = match (self.was_overbought, self.was_oversold) {
            // Exiting overbought downward: mean-reversion short.
            (Some(true), _) if !over => Some(Side::Sell),
            // Exiting oversold upward: mean-reversion long.
            (_, Some(true)) if !under => Some(Side::Buy),
            _ => None,
        };
        self.was_overbought = Some(over);
        self.was_oversold = Some(under);

        let side = signal?;
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
    use trading_core::{AccountState, Candle, NewsItem};
    use chrono::Utc;

    fn candles(closes: &[f64]) -> Vec<Candle> {
        closes
            .iter()
            .map(|&c| Candle { time: Utc::now(), open: c, high: c, low: c, close: c })
            .collect()
    }

    fn flat_account() -> AccountState {
        AccountState { balance: 1000.0, equity: 1000.0, open_units: 0.0, entry_price: None }
    }

    #[test]
    fn rsi_math_needs_data_and_bounds_0_100() {
        assert_eq!(Rsi::rsi(&[], 14), None);
        assert_eq!(Rsi::rsi(&[1.0; 10], 14), None);
        // Steady climb: only gains -> 100.
        let up: Vec<f64> = (0..20).map(|i| 1.0 + i as f64 * 0.01).collect();
        assert_eq!(Rsi::rsi(&up, 14), Some(100.0));
        // Steady fall: only losses -> 0.
        let down: Vec<f64> = (0..20).map(|i| 2.0 - i as f64 * 0.01).collect();
        assert_eq!(Rsi::rsi(&down, 14), Some(0.0));
    }

    #[tokio::test]
    async fn buys_on_exit_from_oversold() {
        // 15 falling closes drive RSI to 0, then 3 rising closes exit it.
        let mut closes: Vec<f64> = (0..15).map(|i| 2.0 - i as f64 * 0.05).collect();
        closes.extend([1.35, 1.45, 1.60]);
        let history = candles(&closes);
        let account = flat_account();
        let news: Vec<NewsItem> = Vec::new();
        let mut strat = Rsi::new("t", 5, 70.0, 30.0, 10.0);
        let mut saw_buy = false;
        for end in 6..=history.len() {
            let ctx = MarketContext {
                symbol: "EUR_USD",
                history: &history[..end],
                recent_news: &news,
                account,
            };
            if matches!(strat.decide(&ctx).await, Some(o) if o.side == Side::Buy) {
                saw_buy = true;
            }
        }
        assert!(saw_buy, "expected a Buy on oversold exit");
    }

    #[tokio::test]
    async fn flat_market_emits_nothing() {
        let history = candles(&[1.0; 30]);
        let account = flat_account();
        let news: Vec<NewsItem> = Vec::new();
        let mut strat = Rsi::new("t", 5, 70.0, 30.0, 10.0);
        for end in 6..=history.len() {
            let ctx = MarketContext {
                symbol: "EUR_USD",
                history: &history[..end],
                recent_news: &news,
                account,
            };
            assert!(strat.decide(&ctx).await.is_none(), "flat must not signal");
        }
    }

    #[tokio::test]
    async fn open_position_blocks_without_opt_in() {
        // Deeply oversold then recovering, but an open long exists.
        let mut closes: Vec<f64> = (0..15).map(|i| 2.0 - i as f64 * 0.05).collect();
        closes.extend([1.35, 1.45, 1.60]);
        let history = candles(&closes);
        let account = AccountState { balance: 1000.0, equity: 1000.0, open_units: 10.0, entry_price: Some(1.0) };
        let news: Vec<NewsItem> = Vec::new();
        let mut strat = Rsi::new("t", 5, 70.0, 30.0, 10.0);
        let mut saw_signal_window = false;
        for end in 6..=history.len() {
            let ctx = MarketContext {
                symbol: "EUR_USD",
                history: &history[..end],
                recent_news: &news,
                account,
            };
            if strat.decide(&ctx).await.is_some() {
                saw_signal_window = true;
            }
        }
        // The oversold-exit window passes while blocked the whole time.
        assert!(!saw_signal_window, "pyramiding must be blocked by default");
    }
}
