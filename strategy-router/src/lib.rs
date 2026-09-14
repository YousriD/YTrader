//! Regime router, Tier 2 (P2-6): pick WHICH tested strategy runs next.
//!
//! Two brains, one trait — this is what "works with and without LLMs" means:
//! - [`RuleRouter`]: deterministic trend/range classifier in pure code.
//!   No key, no network, fully testable. THE DEFAULT brain.
//! - [`LlmRouter`]: asks an LLM to pick a regime from news/stats.
//!   Same failure semantics as `strategy-llm` (threshold → cooldown).
//!   Degrades to the rule brain, never to a stall.
//!
//! [`RouterStrategy`] chains them: LLM pick (validated) → rule pick →
//! configured default. Unknown names fall back instead of erroring.
//! Regime keys are `"trending"` / `"ranging"`; the `regimes` table maps
//! them to candidate names. Non-selected candidates still see every
//! tick (warm-keeping) so a switch never starts from stale state.

pub mod rule;
pub mod llm;
pub mod router;

pub use rule::RuleRouter;
pub use llm::LlmRouter;
pub use router::RouterStrategy;

use std::collections::HashMap;

use async_trait::async_trait;
use trading_core::MarketContext;

/// A regime-picking brain. Returns a regime key (`"trending"` /
/// `"ranging"` by convention) or `None` to abstain (→ next fallback).
/// Every brain MUST be total: no panics, no hangs, no empty-history
/// crashes — the router consults it on live ticks.
#[async_trait]
pub trait RouterBrain: Send + Sync {
    async fn select(&mut self, ctx: &MarketContext<'_>, regimes: &HashMap<String, String>) -> Option<String>;
    fn is_healthy(&self) -> bool;
    /// `"rule"` / `"llm"` — logged at build time so runs state their brain.
    fn kind(&self) -> &'static str;
}
