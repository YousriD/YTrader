use async_trait::async_trait;
use chrono::{DateTime, Utc};
use news_calendar::{CalendarEvent, CalendarFeed, Impact};
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

/// Scheduled-event gate: suppresses entries while a high-impact
/// calendar event for either traded currency is within ±`window`
/// of now (rates, CPI, NFP — the spikes that stop out tight systems).
///
/// Owns a [`CalendarFeed`] polled at most every 50 ticks; event times
/// come from the calendar itself, so this works even though calendar
/// headlines never cross a sentiment threshold. Feed death degrades to
/// no-gate (trade on), never to a halt. Currencies derive from the
/// agent's symbol at decide time (`EUR_USD` → EUR + USD).
pub struct CalendarGate {
    name: String,
    inner: Box<dyn Strategy>,
    feed: Option<CalendarFeed>,
    window_minutes: u32,
    snapshot: Vec<CalendarEvent>,
    tick_count: u64,
}

impl CalendarGate {
    pub fn new(inner: Box<dyn Strategy>, inner_name: &str, window_minutes: u32) -> Self {
        Self {
            name: format!("{inner_name}-calgate{window_minutes}"),
            inner,
            feed: Some(CalendarFeed::new()),
            window_minutes,
            snapshot: Vec::new(),
            tick_count: 0,
        }
    }

    /// Test/prefetch constructor: fixed event list, no fetching ever.
    pub fn with_snapshot(
        inner: Box<dyn Strategy>,
        inner_name: &str,
        events: Vec<CalendarEvent>,
        window_minutes: u32,
    ) -> Self {
        Self {
            name: format!("{inner_name}-calgate{window_minutes}"),
            inner,
            feed: None,
            window_minutes,
            snapshot: events,
            tick_count: 0,
        }
    }

    pub fn pending_events(&self) -> usize {
        self.snapshot.len()
    }
}

/// Currencies an agent's symbol exposes to calendar risk.
/// `EUR_USD` → `[EUR, USD]`; bare 6-letter codes split in half.
pub fn currencies_of(symbol: &str) -> Vec<String> {
    if let Some((a, b)) = symbol.split_once(['_', '/', '-']) {
        vec![a.to_uppercase(), b.to_uppercase()]
    } else if symbol.len() == 6 {
        vec![symbol[..3].to_uppercase(), symbol[3..].to_uppercase()]
    } else {
        vec![symbol.to_uppercase()]
    }
}

/// True when any High+ event for these currencies lands inside ±window.
pub fn suppressed(
    events: &[CalendarEvent],
    now: DateTime<Utc>,
    window_minutes: u32,
    currencies: &[String],
) -> bool {
    let window = chrono::Duration::minutes(window_minutes as i64);
    events.iter().any(|e| {
        e.impact >= Impact::High
            && currencies.iter().any(|c| c == &e.country)
            && (e.date - now).abs() <= window
    })
}

#[async_trait]
impl Strategy for CalendarGate {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_healthy(&self) -> bool {
        self.inner.is_healthy()
    }

    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        self.tick_count += 1;
        // Throttled refresh: the calendar moves hourly, not per tick.
        if self.tick_count % 50 == 1 {
            if let Some(feed) = self.feed.as_mut() {
                for e in feed.fetch_events().await {
                    let key = (e.date, e.country.clone(), e.title.clone());
                    if !self.snapshot.iter().any(|s| (s.date, s.country.clone(), s.title.clone()) == key) {
                        self.snapshot.push(e);
                    }
                }
                if self.snapshot.len() > 500 {
                    let excess = self.snapshot.len() - 500;
                    self.snapshot.drain(..excess);
                }
            }
        }
        let now = Utc::now();
        let window = chrono::Duration::minutes(self.window_minutes as i64);
        self.snapshot.retain(|e| e.date >= now - window);
        if suppressed(&self.snapshot, now, self.window_minutes, &currencies_of(ctx.symbol)) {
            return None;
        }
        self.inner.decide(ctx).await
    }
}

#[cfg(test)]
mod calendar_tests {
    use super::*;
    use trading_core::{AccountState, Candle, Side};

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

    fn history() -> Vec<Candle> {
        vec![Candle { time: Utc::now(), open: 1.0, high: 1.0, low: 1.0, close: 1.0 }; 5]
    }

    fn ctx_for<'a>(history: &'a [Candle]) -> MarketContext<'a> {
        MarketContext {
            symbol: "EUR_USD",
            history,
            recent_news: &[],
            account: AccountState { balance: 1000.0, equity: 1000.0, open_units: 0.0, entry_price: None },
        }
    }

    fn event(country: &str, minutes_from_now: i64, impact: Impact) -> CalendarEvent {
        CalendarEvent {
            country: country.to_string(),
            title: "Test release".to_string(),
            date: Utc::now() + chrono::Duration::minutes(minutes_from_now),
            impact,
            forecast: String::new(),
            previous: String::new(),
        }
    }

    #[test]
    fn currencies_split_symbols() {
        assert_eq!(currencies_of("EUR_USD"), vec!["EUR", "USD"]);
        assert_eq!(currencies_of("eurusd"), vec!["EUR", "USD"]);
        assert_eq!(currencies_of("X"), vec!["X"]);
    }

    #[test]
    fn suppression_needs_high_impact_right_currency_right_time() {
        let now = Utc::now();
        let cur = vec!["EUR".to_string(), "USD".to_string()];
        // Imminent USD high-impact: suppressed.
        assert!(suppressed(&[event("USD", 5, Impact::High)], now, 30, &cur));
        // Holiday counts too.
        assert!(suppressed(&[event("EUR", -10, Impact::Holiday)], now, 30, &cur));
        // Too far away: passes.
        assert!(!suppressed(&[event("USD", 120, Impact::High)], now, 30, &cur));
        // Already over (outside window): passes.
        assert!(!suppressed(&[event("USD", -60, Impact::High)], now, 30, &cur));
        // Wrong currency: passes.
        assert!(!suppressed(&[event("JPY", 5, Impact::High)], now, 30, &cur));
        // Medium impact: passes (timing-only gate, High+ only).
        assert!(!suppressed(&[event("USD", 5, Impact::Medium)], now, 30, &cur));
    }

    #[tokio::test]
    async fn gate_blocks_then_releases() {
        let h = history();
        // USD CPI in 5 minutes, window 30: blocked.
        let mut gate = CalendarGate::with_snapshot(
            Box::new(AlwaysBuy),
            "inner",
            vec![event("USD", 5, Impact::High)],
            30,
        );
        assert!(gate.decide(&ctx_for(&h)).await.is_none());
        // Same gate, event long past its window: inner flows.
        let mut gate = CalendarGate::with_snapshot(
            Box::new(AlwaysBuy),
            "inner",
            vec![event("USD", -120, Impact::High)],
            30,
        );
        assert!(gate.decide(&ctx_for(&h)).await.is_some());
        // Unrelated currency: flows.
        let mut gate = CalendarGate::with_snapshot(
            Box::new(AlwaysBuy),
            "inner",
            vec![event("JPY", 5, Impact::High)],
            30,
        );
        assert!(gate.decide(&ctx_for(&h)).await.is_some());
    }

    #[tokio::test]
    async fn snapshot_gate_never_touches_network() {
        // feed: None by construction — would fail compile if decide
        // required it. If this returns, no fetch happened.
        let h = history();
        let mut gate =
            CalendarGate::with_snapshot(Box::new(AlwaysBuy), "inner", Vec::new(), 30);
        assert_eq!(gate.pending_events(), 0);
        assert!(gate.decide(&ctx_for(&h)).await.is_some());
    }
}
