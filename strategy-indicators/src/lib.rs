//! Indicator strategy registry (P1-2): one-strategy-per-agent plugins
//! that all obey the same inventory rules — no pyramiding by default,
//! sizes clamped — so agents can't fight their own risk layer.
//!
//! - [`Rsi`]: mean-reversion on Wilder's RSI exits.
//! - [`DonchianBreakout`]: trend breakout on channel extremes.
//! - [`AtrSizer`]: decorator scaling any strategy's size to volatility.
//! - [`NewsGate`]: decorator suppressing entries after strong news.
//! - [`inventory`]: the shared guards every strategy applies.

pub mod donchian;
pub mod gate;
pub mod inventory;
pub mod rsi;
pub mod sizing;

pub use donchian::DonchianBreakout;
pub use gate::NewsGate;
pub use rsi::Rsi;
pub use sizing::AtrSizer;
