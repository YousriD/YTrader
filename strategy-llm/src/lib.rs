use async_trait::async_trait;
use serde::Deserialize;
use trading_core::{MarketContext, Order, Side, Strategy};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";

/// Consecutive failures (network error, timeout, bad response, rate
/// limit) before we stop calling the API and enter cooldown.
const FAILURE_THRESHOLD: u32 = 2;
/// How many ticks to wait before trying the API again after tripping
/// the failure threshold. Simple fixed backoff -- good enough for an
/// MVP; swap for exponential backoff if you're hammering a strict
/// rate limit.
const COOLDOWN_TICKS: u32 = 30;

/// A strategy whose "brain" is an LLM call. Deliberately does NOT call
/// the API every tick -- that would be slow, expensive, and pointless
/// (FX doesn't need a fresh opinion every 50ms). It evaluates every
/// `eval_every_n_ticks` ticks, and holds on every other tick.
///
/// Reachability is tracked explicitly: repeated failures trip a
/// cooldown, `is_healthy()` reports false during that cooldown, and a
/// wrapping `HybridStrategy` uses that signal to fall back to an
/// algorithmic decision instead of the agent going idle.
pub struct LlmStrategy {
    name: String,
    api_key: String,
    model: String,
    units: f64,
    client: reqwest::Client,
    persona: String,
    eval_every_n_ticks: u32,
    tick_counter: u32,
    consecutive_failures: u32,
    cooldown_ticks_remaining: u32,
}

#[derive(Deserialize)]
struct Decision {
    action: String, // "buy" | "sell" | "hold"
    #[allow(dead_code)]
    reasoning: String,
}

impl LlmStrategy {
    pub fn new(name: impl Into<String>, api_key: String, units: f64, persona: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            api_key,
            model: "claude-sonnet-4-6".to_string(),
            units,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .expect("failed to build HTTP client"),
            persona: persona.into(),
            eval_every_n_ticks: 10,
            tick_counter: 0,
            consecutive_failures: 0,
            cooldown_ticks_remaining: 0,
        }
    }

    async fn ask_claude(&self, prompt: &str) -> Result<Decision, String> {
        let body = serde_json::json!({
            "model": self.model,
            "max_tokens": 300,
            "system": "You are an FX trading agent. Respond with ONLY a JSON object: \
                       {\"action\": \"buy\"|\"sell\"|\"hold\", \"reasoning\": \"<one sentence>\"}. \
                       No markdown, no extra text.",
            "messages": [{"role": "user", "content": prompt}]
        });

        let resp = self
            .client
            .post(ANTHROPIC_URL)
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

        let json: serde_json::Value = resp.json().await.map_err(|e| format!("bad response body: {e}"))?;
        let text = json["content"][0]["text"]
            .as_str()
            .ok_or_else(|| "no text content in response".to_string())?;
        let cleaned = text.trim().trim_start_matches("```json").trim_end_matches("```");
        serde_json::from_str::<Decision>(cleaned).map_err(|e| format!("failed to parse decision JSON: {e}"))
    }
}

#[async_trait]
impl Strategy for LlmStrategy {
    fn name(&self) -> &str {
        &self.name
    }

    fn is_healthy(&self) -> bool {
        self.cooldown_ticks_remaining == 0
    }

    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order> {
        self.tick_counter += 1;

        // Cooling down after repeated failures: don't even attempt the
        // call, just let the tick pass. `is_healthy()` is false during
        // this window so HybridStrategy knows to use its fallback.
        if self.cooldown_ticks_remaining > 0 {
            self.cooldown_ticks_remaining -= 1;
            return None;
        }

        // Not due for a fresh opinion yet -- this is a deliberate hold,
        // not a failure, so is_healthy() stays true here.
        if self.tick_counter % self.eval_every_n_ticks != 0 {
            return None;
        }

        let last_closes: Vec<f64> = ctx.history.iter().rev().take(20).map(|c| c.close).collect();
        let news_summary: Vec<String> = ctx
            .recent_news
            .iter()
            .map(|n| format!("- {} (sentiment: {:.1})", n.headline, n.sentiment.unwrap_or(0.0)))
            .collect();

        let prompt = format!(
            "Persona: {persona}\n\
             Symbol: {symbol}\n\
             Balance: ${balance:.2}, Equity: ${equity:.2}, Open units: {open_units}\n\
             Last 20 closes (most recent first): {closes:?}\n\
             Recent news:\n{news}\n\
             Decide: buy, sell, or hold.",
            persona = self.persona,
            symbol = ctx.symbol,
            balance = ctx.account.balance,
            equity = ctx.account.equity,
            open_units = ctx.account.open_units,
            closes = last_closes,
            news = if news_summary.is_empty() { "(none)".to_string() } else { news_summary.join("\n") }
        );

        match self.ask_claude(&prompt).await {
            Ok(decision) => {
                self.consecutive_failures = 0;
                match decision.action.as_str() {
                    "buy" => Some(Order { symbol: ctx.symbol.to_string(), side: Side::Buy, units: self.units }),
                    "sell" => Some(Order { symbol: ctx.symbol.to_string(), side: Side::Sell, units: self.units }),
                    _ => None,
                }
            }
            Err(reason) => {
                eprintln!("[{}] LLM call failed ({reason}) -- {}/{} consecutive failures", self.name, self.consecutive_failures + 1, FAILURE_THRESHOLD);
                self.consecutive_failures += 1;
                if self.consecutive_failures >= FAILURE_THRESHOLD {
                    eprintln!("[{}] entering cooldown for {COOLDOWN_TICKS} ticks -- falling back to algorithmic decisions", self.name);
                    self.cooldown_ticks_remaining = COOLDOWN_TICKS;
                }
                None
            }
        }
    }
}
