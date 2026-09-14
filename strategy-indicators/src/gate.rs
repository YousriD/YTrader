use async_trait::async_trait;
use chrono::{DateTime, Utc};
use trading_core::{MarketContext, Order, Strategy};

/// News gate: a transparent decorator that suppresses entries for
/// `cooldown_ticks` after any news item with `|sentiment| >= threshold`.
///
/// This is the "scheduled high-impact event" guard for FX (rates, CPI,
/// NFP): with a real calendar feed, pre-event items carry extreme
/// sentiment and the gate keeps the agent flat through the spike.
/// With the mock feed it trips on random strong headlines — same code
/// path, less meaning. Feed failure degrades to no-gate (no news, no
/// suppression), never to a trading halt.
pub struct NewsGate {
    name: String,
    inner: Box<dyn Strategy>,
    cooldown_ticks: u32,
    sentiment_threshold: f64,
    quiet_remaining: u32,
    last_seen: Option<(DateTime<Utc>, String)>,
}

impl NewsGate {
    pub fn new(inner: Box<dyn Strategy>, inner_name: &str, cooldown_ticks: u32, sentiment_threshold: f64) -> Self {
        Self {
            name: format!("{inner_name}-newsgate"),
            inner,
            cooldown_ticks,
            sentiment_threshold: sentiment_threshold.abs().clamp(0.0, 1.0),
            quiet_remaining: 0,
            last_seen: None,
        }
    }

    pub fn inner_mut(&mut self) -> &mut (dyn Strategy + 'static) {
        &mut *self.inner
    }

    pub fn quiet_remaining(&self) -> u32 {
        self.quiet_remaining
    }
}

#[async_trait]
impl Strategy for NewsGate {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_healthy(&self) -> bool {
        self.inner.is_healthy()
    }

    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        if let Some(latest) = ctx.recent_news.last() {
            let key = (latest.time, latest.headline.clone());
            if self.last_seen.as_ref() != Some(&key) {
                self.last_seen = Some(key);
                if latest.sentiment.unwrap_or(0.0).abs() >= self.sentiment_threshold {
                    self.quiet_remaining = self.cooldown_ticks;
                }
            }
        }
        if self.quiet_remaining > 0 {
            self.quiet_remaining -= 1;
            return None;
        }
        self.inner.decide(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trading_core::{AccountState, Candle, NewsItem, Side};

    struct AlwaysBuy;

    #[async_trait]
    impl Strategy for AlwaysBuy {
        fn name(&self) -> &str {
            "always-buy"
        }
        async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
            Some(Order { symbol: ctx.symbol.to_string(), side: Side::Buy, units: 10.0 })
        }
    }

    fn ctx_for<'a>(
        history: &'a [Candle],
        news: &'a [trading_core::NewsItem],
    ) -> MarketContext<'a> {
        MarketContext {
            symbol: "EUR_USD",
            history,
            recent_news: news,
            account: AccountState { balance: 1000.0, equity: 1000.0, open_units: 0.0, entry_price: None },
        }
    }

    fn candle() -> Candle {
        Candle { time: Utc::now(), open: 1.0, high: 1.0, low: 1.0, close: 1.0 }
    }

    fn news(headline: &str, sentiment: Option<f64>) -> NewsItem {
        NewsItem { time: Utc::now(), headline: headline.to_string(), sentiment }
    }

    #[tokio::test]
    async fn passes_through_with_no_news() {
        let history = vec![candle(); 5];
        let no_news: Vec<NewsItem> = Vec::new();
        let mut gate = NewsGate::new(Box::new(AlwaysBuy), "inner", 3, 0.5);
        assert!(gate.decide(&ctx_for(&history, &no_news)).await.is_some());
    }

    #[tokio::test]
    async fn suppresses_for_cooldown_after_strong_news_then_recovers() {
        let history = vec![candle(); 5];
        let hot = vec![news("Central bank shocks with hike", Some(0.9))];
        let no_news: Vec<NewsItem> = Vec::new();
        let mut gate = NewsGate::new(Box::new(AlwaysBuy), "inner", 2, 0.5);
        // Trips: suppressed for exactly `cooldown_ticks` ticks...
        assert!(gate.decide(&ctx_for(&history, &hot)).await.is_none());
        assert_eq!(gate.quiet_remaining(), 1);
        assert!(gate.decide(&ctx_for(&history, &hot)).await.is_none());
        // ...then the same news item doesn't re-trip, inner flows again.
        assert!(gate.decide(&ctx_for(&history, &hot)).await.is_some());
        // Unscored/weak news never trips.
        let mut gate = NewsGate::new(Box::new(AlwaysBuy), "inner", 2, 0.5);
        let weak = vec![news("Calm markets", Some(0.1)), news("No score", None)];
        for item in &weak {
            let one = vec![NewsItem { time: item.time, headline: item.headline.clone(), sentiment: item.sentiment }];
            assert!(gate.decide(&ctx_for(&history, &one)).await.is_some());
        }
        assert!(gate.decide(&ctx_for(&history, &no_news)).await.is_some());
    }
}
