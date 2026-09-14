use async_trait::async_trait;
use trading_core::{Candle, MarketContext, Order, Strategy};

use crate::inventory::clamp_units;

/// ATR-based position sizer: a transparent decorator over any inner
/// strategy (boxed so `config.toml` can nest it around any `kind`).
/// Keeps the inner strategy's side, replaces its size with
/// `equity * risk_pct / atr`, clamped to `[1.0, max_units]`.
///
/// Honest small-account behavior: e.g. 2% of $50 is $1, so at ATR 0.002
/// this emits ~333 units — far below the sample config's fixed 1000.
/// If ATR isn't computable yet (too little history), the inner order
/// passes through clamped to `max_units`.
pub struct AtrSizer {
    name: String,
    inner: Box<dyn Strategy>,
    period: usize,
    risk_pct: f64,
    max_units: f64,
}

impl AtrSizer {
    pub fn new(inner: Box<dyn Strategy>, inner_name: &str, period: usize, risk_pct: f64, max_units: f64) -> Self {
        Self {
            name: format!("{inner_name}-atr{period}"),
            inner,
            period,
            risk_pct: risk_pct.clamp(0.0, 1.0),
            max_units: max_units.max(1.0),
        }
    }

    pub fn inner_mut(&mut self) -> &mut (dyn Strategy + 'static) {
        &mut *self.inner
    }

    /// Wilder's ATR over candle true-ranges. Needs `period + 1` candles.
    /// Pure function of history — unit-testable.
    pub fn atr(history: &[Candle], period: usize) -> Option<f64> {
        let n = history.len();
        if period == 0 || n < period + 1 {
            return None;
        }
        let tr = |i: usize| {
            (history[i].high - history[i].low)
                .max((history[i].high - history[i - 1].close).abs())
                .max((history[i].low - history[i - 1].close).abs())
        };
        let p = period as f64;
        let mut avg: f64 = (1..=period).map(tr).sum::<f64>() / p;
        for i in (period + 1)..n {
            avg = (avg * (p - 1.0) + tr(i)) / p;
        }
        Some(avg)
    }
}

#[async_trait]
impl Strategy for AtrSizer {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_healthy(&self) -> bool {
        self.inner.is_healthy()
    }

    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        let mut order = self.inner.decide(ctx).await?;
        order.units = match Self::atr(ctx.history, self.period) {
            Some(a) if a > 0.0 => clamp_units(ctx.account.equity * self.risk_pct / a, self.max_units),
            _ => clamp_units(order.units, self.max_units),
        };
        Some(order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trading_core::{AccountState, NewsItem, Side};
    use chrono::Utc;

    struct AlwaysBuy {
        units: f64,
    }

    #[async_trait]
    impl Strategy for AlwaysBuy {
        fn name(&self) -> &str {
            "always-buy"
        }
        async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
            Some(Order { symbol: ctx.symbol.to_string(), side: Side::Buy, units: self.units })
        }
    }

    fn candle(o: f64, h: f64, l: f64, c: f64) -> Candle {
        Candle { time: Utc::now(), open: o, high: h, low: l, close: c }
    }

    fn ctx_for<'a>(history: &'a [Candle], news: &'a [NewsItem], equity: f64) -> MarketContext<'a> {
        MarketContext {
            symbol: "EUR_USD",
            history,
            recent_news: news,
            account: AccountState { balance: equity, equity, open_units: 0.0, entry_price: None },
        }
    }

    #[test]
    fn atr_needs_data_and_tracks_volatility() {
        assert_eq!(AtrSizer::atr(&[], 14), None);
        let calm: Vec<Candle> = (0..30).map(|i| {
            let p = 1.0 + i as f64 * 0.0001;
            candle(p, p + 0.0002, p - 0.0002, p)
        }).collect();
        let wild: Vec<Candle> = (0..30).map(|i| {
            let p = 1.0 + i as f64 * 0.0001;
            candle(p, p + 0.02, p - 0.02, p)
        }).collect();
        let a_calm = AtrSizer::atr(&calm, 14).unwrap();
        let a_wild = AtrSizer::atr(&wild, 14).unwrap();
        assert!(a_calm > 0.0 && a_wild > a_calm * 10.0);
    }

    #[tokio::test]
    async fn sizes_down_as_volatility_rises() {
        let news: Vec<NewsItem> = Vec::new();
        let mk = |range: f64| -> Vec<Candle> {
            (0..30).map(|i| {
                let p = 1.0 + i as f64 * 0.0001;
                candle(p, p + range, p - range, p)
            }).collect()
        };
        let calm = mk(0.0002);
        let wild = mk(0.02);
        let mut sizer = AtrSizer::new(Box::new(AlwaysBuy { units: 10_000.0 }), "inner", 14, 0.02, 5_000.0);
        let u_calm = sizer.decide(&ctx_for(&calm, &news, 1_000.0)).await.unwrap().units;
        let u_wild = sizer.decide(&ctx_for(&wild, &news, 1_000.0)).await.unwrap().units;
        assert!(u_calm > u_wild, "calm={u_calm} wild={u_wild}");
        assert!(u_calm <= 5_000.0 && u_wild >= 1.0);
        // $1000 * 2% / ~0.0004 ≈ 50k -> capped at max_units.
        assert_eq!(u_calm, 5_000.0);
    }

    #[tokio::test]
    async fn passes_through_clamped_before_atr_ready() {
        let news: Vec<NewsItem> = Vec::new();
        let history = vec![candle(1.0, 1.0, 1.0, 1.0); 5];
        let mut sizer = AtrSizer::new(Box::new(AlwaysBuy { units: 10_000.0 }), "inner", 14, 0.02, 500.0);
        let got = sizer.decide(&ctx_for(&history, &news, 1_000.0)).await.unwrap();
        assert_eq!(got.units, 500.0);
        assert_eq!(got.side, Side::Buy);
    }
}
