use async_trait::async_trait;
use chrono::Utc;
use rand::Rng;
use trading_core::{Candle, MarketFeed};

/// Random-walk price generator. Good enough to exercise the whole
/// pipeline (strategies, risk manager, kill/split logic) before you've
/// wired up a real feed. Replace with `feed-oanda` (websocket streaming)
/// for real data — same `MarketFeed` trait.
pub struct MockFeed {
    price: f64,
    vol: f64, // per-tick volatility
}

impl MockFeed {
    pub fn new(start_price: f64, vol: f64) -> Self {
        Self {
            price: start_price,
            vol,
        }
    }
}

#[async_trait]
impl MarketFeed for MockFeed {
    async fn next_price(&mut self, _symbol: &str) -> Option<Candle> {
        let mut rng = rand::thread_rng();
        let drift: f64 = rng.gen_range(-self.vol..self.vol);
        self.price = (self.price + drift).max(0.0001);

        let open = self.price - drift;
        let high = open.max(self.price) + rng.gen_range(0.0..self.vol / 2.0);
        let low = open.min(self.price) - rng.gen_range(0.0..self.vol / 2.0);

        Some(Candle {
            time: Utc::now(),
            open,
            high,
            low,
            close: self.price,
        })
    }
}
