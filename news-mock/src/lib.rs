use async_trait::async_trait;
use chrono::Utc;
use rand::Rng;
use trading_core::{NewsFeed, NewsItem};

const HEADLINES: &[(&str, f64)] = &[
    ("Central bank signals rate hike", 0.6),
    ("Inflation data comes in hotter than expected", 0.3),
    ("Trade tensions escalate between major economies", -0.5),
    ("Strong jobs report beats forecasts", 0.7),
    ("Central bank holds rates, cautious tone", -0.1),
    ("Manufacturing PMI slips into contraction", -0.6),
    ("Currency intervention rumors circulate", -0.3),
];

/// Emits a random headline roughly every `every_n_ticks` calls.
/// Replace with a real news-mock/news-live crate hitting a real
/// news/calendar API — same `NewsFeed` trait, orchestrator unchanged.
pub struct MockNewsFeed {
    every_n_ticks: u32,
    counter: u32,
}

impl MockNewsFeed {
    pub fn new(every_n_ticks: u32) -> Self {
        Self {
            every_n_ticks,
            counter: 0,
        }
    }
}

#[async_trait]
impl NewsFeed for MockNewsFeed {
    async fn next_headline(&mut self) -> Option<NewsItem> {
        self.counter += 1;
        if self.counter % self.every_n_ticks != 0 {
            return None;
        }
        let mut rng = rand::thread_rng();
        let (headline, base_sentiment) = HEADLINES[rng.gen_range(0..HEADLINES.len())];
        let jitter: f64 = rng.gen_range(-0.1..0.1);
        Some(NewsItem {
            time: Utc::now(),
            headline: headline.to_string(),
            sentiment: Some((base_sentiment + jitter).clamp(-1.0, 1.0)),
        })
    }
}
