use async_trait::async_trait;
use trading_core::{MarketContext, Order, Strategy};

/// Wraps a primary strategy (typically LLM-driven) with an algorithmic
/// fallback. This is what makes "the system must keep operating if the
/// AI is unreachable or rate-limited" an actual guarantee rather than a
/// hope: every tick, if the primary reports itself unhealthy, the
/// fallback's decision is used instead — the agent never idles waiting
/// on a call that isn't coming back.
///
/// The fallback always sees every tick (even ticks where it isn't
/// used), so its internal state (e.g. a moving-average window) stays
/// warm and its first real decision after a failover isn't working
/// from stale/no data.
pub struct HybridStrategy {
    name: String,
    primary: Box<dyn Strategy>,
    fallback: Box<dyn Strategy>,
    currently_on_fallback: bool,
}

impl HybridStrategy {
    pub fn new(name: impl Into<String>, primary: Box<dyn Strategy>, fallback: Box<dyn Strategy>) -> Self {
        Self {
            name: name.into(),
            primary,
            fallback,
            currently_on_fallback: false,
        }
    }
}

#[async_trait]
impl Strategy for HybridStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_healthy(&self) -> bool {
        // The hybrid itself is always "healthy" from the outside — it
        // always produces a usable decision, that's the whole point.
        true
    }

    fn active_strategy(&self) -> Option<&str> {
        // Surface whichever side is actually deciding, so a router
        // nested as primary stays visible through the hybrid wrapper.
        if self.primary.is_healthy() {
            self.primary.active_strategy()
        } else {
            self.fallback.active_strategy()
        }
    }

    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        let primary_result = self.primary.decide(ctx).await;
        let fallback_healthy_path = !self.primary.is_healthy();

        if fallback_healthy_path {
            if !self.currently_on_fallback {
                eprintln!("[{}] primary unreachable — switching to algorithmic fallback", self.name);
                self.currently_on_fallback = true;
            }
            self.fallback.decide(ctx).await
        } else {
            if self.currently_on_fallback {
                eprintln!("[{}] primary reachable again — switching back", self.name);
                self.currently_on_fallback = false;
            }
            // Keep the fallback's internal state warm even when unused.
            let _ = self.fallback.decide(ctx).await;
            primary_result
        }
    }
}
