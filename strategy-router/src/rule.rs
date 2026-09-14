use std::collections::HashMap;

use async_trait::async_trait;
use trading_core::MarketContext;

use super::RouterBrain;

/// Deterministic trend/range classifier (the no-LLM brain).
/// slope = per-tick drift over `window` closes, scaled by mean absolute
/// move over the same window. `|slope|/scale > 0.3` → trending.
/// Flat or data-starved history → ranging / abstain respectively.
pub struct RuleRouter {
    window: usize,
}

impl RuleRouter {
    pub fn new(window: usize) -> Self {
        Self { window: window.max(2) }
    }

    /// Pure function of closes — unit-testable without a strategy.
    /// Returns `None` when there isn't enough history to judge.
    pub fn classify(closes: &[f64], window: usize) -> Option<&'static str> {
        if window < 2 || closes.len() < window + 1 {
            return None;
        }
        let w = &closes[closes.len() - window - 1..];
        let drift = (w[window] - w[0]) / window as f64;
        let scale: f64 =
            w.windows(2).map(|p| (p[1] - p[0]).abs()).sum::<f64>() / window as f64;
        if scale == 0.0 {
            return Some("ranging");
        }
        if drift.abs() / scale > 0.3 {
            Some("trending")
        } else {
            Some("ranging")
        }
    }
}

#[async_trait]
impl RouterBrain for RuleRouter {
    async fn select(&mut self, ctx: &MarketContext<'_>, _regimes: &HashMap<String, String>) -> Option<String> {
        let closes: Vec<f64> = ctx.history.iter().map(|c| c.close).collect();
        Self::classify(&closes, self.window).map(|s| s.to_string())
    }

    fn is_healthy(&self) -> bool {
        true
    }

    fn kind(&self) -> &'static str {
        "rule"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steady_climb_is_trending() {
        let closes: Vec<f64> = (0..30).map(|i| 1.0 + i as f64 * 0.01).collect();
        assert_eq!(RuleRouter::classify(&closes, 20), Some("trending"));
        let down: Vec<f64> = (0..30).map(|i| 2.0 - i as f64 * 0.01).collect();
        assert_eq!(RuleRouter::classify(&down, 20), Some("trending"));
    }

    #[test]
    fn oscillation_and_flat_are_ranging() {
        let mut wave: Vec<f64> = Vec::new();
        for i in 0..30 {
            wave.push(1.0 + if i % 2 == 0 { 0.05 } else { -0.05 });
        }
        assert_eq!(RuleRouter::classify(&wave, 20), Some("ranging"));
        assert_eq!(RuleRouter::classify(&vec![1.0; 30], 20), Some("ranging"));
    }

    #[test]
    fn starved_history_abstains() {
        assert_eq!(RuleRouter::classify(&[1.0, 2.0], 20), None);
        assert_eq!(RuleRouter::classify(&[], 20), None);
        assert_eq!(RuleRouter::classify(&vec![1.0; 30], 0), None);
        assert_eq!(RuleRouter::classify(&vec![1.0; 30], 1), None);
    }
}
