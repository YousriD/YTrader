//! trading-core
//!
//! Defines the trait boundaries the whole system is built on. Every new
//! broker, price feed, news source, or strategy plugs in by implementing
//! one of these traits — nothing else in the system needs to change.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

// ---------- Market data ----------

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Candle {
    pub time: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// Anything that can produce a stream of prices for a symbol (mock random
/// walk today, OANDA/IB websocket tomorrow) implements this.
#[async_trait]
pub trait MarketFeed: Send + Sync {
    /// Returns the next candle/tick for `symbol`, or None if the feed ended.
    async fn next_price(&mut self, symbol: &str) -> Option<Candle>;
}

// ---------- News ----------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsItem {
    pub time: DateTime<Utc>,
    pub headline: String,
    /// -1.0 (very bearish) .. +1.0 (very bullish). None if unscored.
    pub sentiment: Option<f64>,
}

#[async_trait]
pub trait NewsFeed: Send + Sync {
    async fn next_headline(&mut self) -> Option<NewsItem>;
}

// ---------- Orders / execution ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub symbol: String,
    pub side: Side,
    /// Units of base currency. Keep small for micro accounts.
    pub units: f64,
}

#[derive(Debug, Clone)]
pub struct Fill {
    pub order: Order,
    pub price: f64,
    pub time: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
pub struct AccountState {
    pub balance: f64,
    pub equity: f64,
    pub open_units: f64,
    /// Average entry price of the current open position, if any.
    /// Needed by the risk layer to evaluate stop-loss / take-profit.
    pub entry_price: Option<f64>,
}

#[derive(Debug)]
pub enum BrokerError {
    InsufficientBalance,
    NoPrice,
    Other(String),
}

impl fmt::Display for BrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrokerError::InsufficientBalance => write!(f, "insufficient balance"),
            BrokerError::NoPrice => write!(f, "no current price available"),
            BrokerError::Other(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for BrokerError {}

/// Anything that can execute an order and report account state.
/// A PaperBroker (simulated fills) and a LiveBroker (real API) implement
/// the exact same trait, so an Agent cannot tell — and does not care —
/// which one it's trading against.
#[async_trait]
pub trait Broker: Send + Sync {
    async fn place_order(&mut self, order: Order) -> Result<Fill, BrokerError>;
    fn account_state(&self) -> AccountState;
    /// Mark-to-market price used for equity calculations.
    fn last_price(&self) -> Option<f64>;
    /// Update the broker's internal reference price. For a PaperBroker
    /// this is fed from the mock/replay feed each tick; a live broker
    /// adapter would typically no-op this (it gets its own price stream
    /// directly from the exchange) or use it purely for display.
    fn mark_price(&mut self, price: f64);
    /// Called once on startup (and after any reconnect) before trading
    /// resumes. A paper broker has nothing to reconcile — its state IS
    /// the truth. A live adapter MUST override this to query the real
    /// account/position state from the broker's API and overwrite any
    /// stale local assumptions, so a crash-and-restart can never trade
    /// on a phantom position. This is a hook precisely so that
    /// forgetting it is a compile-time decision, not a 2am surprise.
    async fn reconcile(&mut self) -> Result<(), BrokerError> {
        Ok(())
    }
}

// ---------- Strategy ----------

/// Everything a strategy is allowed to see when deciding what to do.
pub struct MarketContext<'a> {
    pub symbol: &'a str,
    pub history: &'a [Candle],
    pub recent_news: &'a [NewsItem],
    pub account: AccountState,
}

/// A strategy is anything that turns market context into an (optional)
/// order. Algorithmic strategies and LLM-driven strategies are peers —
/// same trait, same trust level, same risk controls wrapped around them.
#[async_trait]
pub trait Strategy: Send + Sync {
    fn name(&self) -> &str;
    async fn decide(&mut self, ctx: &MarketContext<'_>) -> Option<Order>;
    /// Whether this strategy is currently able to make informed decisions.
    /// An algorithmic strategy is always healthy. An LLM-backed strategy
    /// should report `false` while the API is unreachable or rate-limited,
    /// so a wrapping `HybridStrategy` knows to fall back to an algorithmic
    /// decision instead of stalling.
    fn is_healthy(&self) -> bool {
        true
    }
}

// ---------- Run mode ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    Test,
    Live,
}
