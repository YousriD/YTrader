use std::collections::HashMap;

use async_trait::async_trait;
use serde::Deserialize;
use trading_core::MarketContext;

use super::RouterBrain;

const DEFAULT_URL: &str = "https://api.anthropic.com/v1/messages";
const FAILURE_THRESHOLD: u32 = 2;
const COOLDOWN_TICKS: u32 = 30;

/// LLM regime brain: picks `"trending"` / `"ranging"` (or whatever keys
/// the `regimes` table holds) from market context + news. Same failure
/// contract as `strategy-llm`: evaluate every N ticks, hold otherwise;
/// repeated failures trip a cooldown during which `is_healthy()` is
/// false and the router uses the rule brain instead. Bounded 8s calls.
pub struct LlmRouter {
    name: String,
    api_key: String,
    endpoint: String,
    model: String,
    client: reqwest::Client,
    eval_every_n_ticks: u32,
    tick_counter: u32,
    consecutive_failures: u32,
    cooldown_ticks_remaining: u32,
}

#[derive(Deserialize)]
struct RegimeDecision {
    regime: String,
    #[allow(dead_code)]
    reasoning: String,
}

impl LlmRouter {
    pub fn new(name: impl Into<String>, api_key: String, eval_every_n_ticks: u32) -> Self {
        Self::with_endpoint(name, api_key, DEFAULT_URL, "claude-sonnet-4-6", eval_every_n_ticks)
    }

    pub fn with_endpoint(
        name: impl Into<String>,
        api_key: String,
        endpoint: impl Into<String>,
        model: &str,
        eval_every_n_ticks: u32,
    ) -> Self {
        Self {
            name: name.into(),
            api_key,
            endpoint: endpoint.into(),
            model: model.to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .expect("failed to build HTTP client"),
            eval_every_n_ticks: eval_every_n_ticks.max(1),
            tick_counter: 0,
            consecutive_failures: 0,
            cooldown_ticks_remaining: 0,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    async fn ask(&self, prompt: &str) -> Result<RegimeDecision, String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 200,
            "system": "You are an FX regime classifier. Respond with ONLY a JSON object: \
                       {\"regime\": \"<one of the allowed regimes>\", \"reasoning\": \"<one sentence>\"}. \
                       No markdown, no extra text.",
            "messages": [{"role": "user", "content": prompt}]
        });
        let resp = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("network error: {e}"))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err("rate limited".to_string());
        }
        if !resp.status().is_success() {
            return Err(format!("API error: {}", resp.status()));
        }
        let json: serde_json::Value =
            resp.json().await.map_err(|e| format!("bad response body: {e}"))?;
        let text = json["content"][0]["text"]
            .as_str()
            .ok_or_else(|| "no text content in response".to_string())?;
        let cleaned = text.trim().trim_start_matches("```json").trim_end_matches("```");
        serde_json::from_str::<RegimeDecision>(cleaned)
            .map_err(|e| format!("failed to parse regime JSON: {e}"))
    }
}

#[async_trait]
impl RouterBrain for LlmRouter {
    async fn select(&mut self, ctx: &MarketContext<'_>, regimes: &HashMap<String, String>) -> Option<String> {
        self.tick_counter += 1;
        if self.cooldown_ticks_remaining > 0 {
            self.cooldown_ticks_remaining -= 1;
            return None;
        }
        if self.tick_counter % self.eval_every_n_ticks != 0 {
            return None;
        }
        let mut options: Vec<&String> = regimes.keys().collect();
        options.sort();
        let last_closes: Vec<f64> = ctx.history.iter().rev().take(20).map(|c| c.close).collect();
        let prompt = format!(
            "Symbol: {symbol}\n\
             Allowed regimes: {options:?}\n\
             Last 20 closes (most recent first): {closes:?}\n\
             Open units: {open_units}\n\
             Reply with the regime that best describes the market now.",
            symbol = ctx.symbol,
            options = options,
            closes = last_closes,
            open_units = ctx.account.open_units,
        );
        match self.ask(&prompt).await {
            Ok(decision) => {
                self.consecutive_failures = 0;
                Some(decision.regime)
            }
            Err(reason) => {
                eprintln!(
                    "[{}] router LLM call failed ({reason}) -- {}/{} consecutive failures",
                    self.name,
                    self.consecutive_failures + 1,
                    FAILURE_THRESHOLD
                );
                self.consecutive_failures += 1;
                if self.consecutive_failures >= FAILURE_THRESHOLD {
                    eprintln!("[{}] entering cooldown for {COOLDOWN_TICKS} ticks", self.name);
                    self.cooldown_ticks_remaining = COOLDOWN_TICKS;
                }
                None
            }
        }
    }

    fn is_healthy(&self) -> bool {
        self.cooldown_ticks_remaining == 0
    }

    fn kind(&self) -> &'static str {
        "llm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use trading_core::{AccountState, Candle, NewsItem};

    fn ctx_for<'a>(history: &'a [Candle]) -> MarketContext<'a> {
        MarketContext {
            symbol: "EUR_USD",
            history,
            recent_news: &[],
            account: AccountState { balance: 100.0, equity: 100.0, open_units: 0.0, entry_price: None },
        }
    }

    fn history() -> Vec<Candle> {
        vec![Candle { time: Utc::now(), open: 1.0, high: 1.0, low: 1.0, close: 1.0 }; 25]
    }

    fn regimes() -> HashMap<String, String> {
        HashMap::from([
            ("trending".to_string(), "donch".to_string()),
            ("ranging".to_string(), "rsi".to_string()),
        ])
    }

    /// Unreachable endpoint: every call fails → cooldown trips, brain
    /// reports unhealthy, selections abstain. No key, no network needed.
    #[tokio::test]
    async fn repeated_failures_trip_cooldown() {
        let h = history();
        let mut brain = LlmRouter::with_endpoint("t", "bad-key".to_string(), "http://127.0.0.1:9/", "m", 1);
        assert!(brain.is_healthy());
        // eval_every=1: two failing evals trip the threshold.
        assert!(brain.select(&ctx_for(&h), &regimes()).await.is_none());
        assert!(brain.select(&ctx_for(&h), &regimes()).await.is_none());
        assert!(!brain.is_healthy());
        // Cooldown ticks abstain without attempting calls.
        assert!(brain.select(&ctx_for(&h), &regimes()).await.is_none());
    }

    #[tokio::test]
    async fn eval_cadence_holds_between_evals() {
        // Pure-cadence check needs no network: with eval_every=10 the
        // first 9 ticks return None before any call is attempted.
        let h = history();
        let mut brain = LlmRouter::with_endpoint("t", "k".to_string(), "http://127.0.0.1:9/", "m", 10);
        for _ in 0..9 {
            assert!(brain.select(&ctx_for(&h), &regimes()).await.is_none());
        }
        assert!(brain.is_healthy()); // no failures: nothing was attempted
        assert_eq!(brain.consecutive_failures, 0);
    }
}
