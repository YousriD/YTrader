use std::collections::HashMap;

use async_trait::async_trait;
use trading_core::{MarketContext, Order, Strategy};

use super::{RouterBrain, RuleRouter};

/// Tier 2 router: delegates each tick to one tested candidate.
///
/// Fallback chain, in order: LLM pick (validated against `regimes`)
/// → rule-brain pick → configured `default`. The chain is total — it
/// always resolves to a candidate that exists, so `decide` below only
/// ever returns `None` when the chosen candidate itself holds.
/// Non-selected candidates still see every tick (warm-keeping), and
/// `active_strategy()` reports the current pick for regime attribution.
pub struct RouterStrategy {
    name: String,
    brain: Box<dyn RouterBrain>,
    rule: RuleRouter,
    candidates: HashMap<String, Box<dyn Strategy>>,
    regimes: HashMap<String, String>,
    default: String,
    current: String,
}

impl RouterStrategy {
    pub fn new(
        name: impl Into<String>,
        brain: Box<dyn RouterBrain>,
        candidates: HashMap<String, Box<dyn Strategy>>,
        regimes: HashMap<String, String>,
        default: impl Into<String>,
        trend_window: usize,
    ) -> Self {
        let default = default.into();
        let name = name.into();
        let current = default.clone();
        Self { name, brain, rule: RuleRouter::new(trend_window), candidates, regimes, default, current }
    }

    pub fn brain_kind(&self) -> &'static str {
        self.brain.kind()
    }

    fn resolve(&mut self, pick: Option<String>) -> String {
        if let Some(regime) = pick {
            if let Some(candidate) = self.regimes.get(&regime) {
                if self.candidates.contains_key(candidate) {
                    return candidate.clone();
                }
            }
        }
        if self.candidates.contains_key(&self.default) {
            return self.default.clone();
        }
        // Misconfigured tree (default missing): first candidate wins over
        // panicking mid-run. Builder validation should prevent this.
        self.candidates.keys().next().cloned().unwrap_or_default()
    }
}

#[async_trait]
impl Strategy for RouterStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_healthy(&self) -> bool {
        // Always healthy from outside: the chain always resolves.
        true
    }

    fn active_strategy(&self) -> Option<&str> {
        Some(&self.current)
    }

    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        let mut pick = None;
        if self.brain.is_healthy() {
            pick = self.brain.select(ctx, &self.regimes).await;
        }
        if pick.is_none() {
            pick = self.rule.select(ctx, &self.regimes).await;
        }
        self.current = self.resolve(pick);
        // Warm-keeping: every candidate advances on the same context so
        // a switch never starts from stale crossover memory.
        for (name, strat) in self.candidates.iter_mut() {
            if name != &self.current {
                let _ = strat.decide(ctx).await;
            }
        }
        self.candidates.get_mut(&self.current)?.decide(ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use trading_core::{AccountState, Candle, NewsItem, Side};

    struct StubBrain {
        pick: Option<String>,
        healthy: bool,
    }

    #[async_trait]
    impl RouterBrain for StubBrain {
        async fn select(&mut self, _ctx: &MarketContext<'_>, _regimes: &HashMap<String, String>) -> Option<String> {
            self.pick.clone()
        }
        fn is_healthy(&self) -> bool {
            self.healthy
        }
        fn kind(&self) -> &'static str {
            "stub"
        }
    }

    struct AlwaysBuy;
    struct Hold;

    #[async_trait]
    impl Strategy for AlwaysBuy {
        fn name(&self) -> &str {
            "always-buy"
        }
        async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
            Some(Order { symbol: ctx.symbol.to_string(), side: Side::Buy, units: 10.0 })
        }
    }

    #[async_trait]
    impl Strategy for Hold {
        fn name(&self) -> &str {
            "hold"
        }
        async fn decide(&mut self, _ctx: &MarketContext<'_>) -> Option<Order> {
            None
        }
    }

    fn cands() -> HashMap<String, Box<dyn Strategy>> {
        HashMap::from([
            ("trend".to_string(), Box::new(AlwaysBuy) as Box<dyn Strategy>),
            ("range".to_string(), Box::new(Hold) as Box<dyn Strategy>),
        ])
    }

    fn regimes() -> HashMap<String, String> {
        HashMap::from([
            ("trending".to_string(), "trend".to_string()),
            ("ranging".to_string(), "range".to_string()),
        ])
    }

    fn history() -> Vec<Candle> {
        vec![Candle { time: Utc::now(), open: 1.0, high: 1.0, low: 1.0, close: 1.0 }; 30]
    }

    fn ctx_for<'a>(history: &'a [Candle]) -> MarketContext<'a> {
        MarketContext {
            symbol: "EUR_USD",
            history,
            recent_news: &[],
            account: AccountState { balance: 100.0, equity: 100.0, open_units: 0.0, entry_price: None },
        }
    }

    fn router(pick: Option<&str>, healthy: bool) -> RouterStrategy {
        RouterStrategy::new(
            "r",
            Box::new(StubBrain { pick: pick.map(|s| s.to_string()), healthy }),
            cands(),
            regimes(),
            "trend",
            20,
        )
    }

    #[tokio::test]
    async fn valid_pick_routes_and_reports() {
        let h = history();
        let mut r = router(Some("ranging"), true);
        assert!(r.decide(&ctx_for(&h)).await.is_none()); // Hold holds
        assert_eq!(r.active_strategy(), Some("range"));
    }

    #[tokio::test]
    async fn unknown_pick_falls_back_to_default() {
        let h = history();
        let mut r = router(Some("martian-regime"), true);
        let order = r.decide(&ctx_for(&h)).await.expect("default AlwaysBuy fires");
        assert_eq!(order.side, Side::Buy);
        assert_eq!(r.active_strategy(), Some("trend"));
    }

    #[tokio::test]
    async fn unhealthy_brain_uses_rule_fallback() {
        // Flat history → rule says ranging → Hold.
        let h = history();
        let mut r = router(Some("trending"), false);
        assert!(r.decide(&ctx_for(&h)).await.is_none());
        assert_eq!(r.active_strategy(), Some("range"));
        // Climbing history → rule says trending → AlwaysBuy.
        let up: Vec<Candle> = (0..30)
            .map(|i| {
                let p = 1.0 + i as f64 * 0.01;
                Candle { time: Utc::now(), open: p, high: p, low: p, close: p }
            })
            .collect();
        assert!(r.decide(&ctx_for(&up)).await.is_some());
        assert_eq!(r.active_strategy(), Some("trend"));
    }

    #[tokio::test]
    async fn abstain_falls_back_to_rule() {
        let h = history();
        let mut r = router(None, true); // healthy brain, no opinion
        assert!(r.decide(&ctx_for(&h)).await.is_none()); // rule: ranging
        assert_eq!(r.active_strategy(), Some("range"));
    }
}
