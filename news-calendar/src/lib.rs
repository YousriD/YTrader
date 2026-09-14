//! Economic-calendar feed (P2-5): real scheduled-event data, no API key.
//!
//! Source: the ForexFactory weekly calendar JSON (nfs.faireconomy.media),
//! free and keyless. High-impact events (rates, CPI, NFP) drive FX more
//! than headlines, so this feed exists first; headline APIs with LLM
//! sentiment are the documented follow-up.
//!
//! Design notes that matter:
//! - `NewsItem.time` is the EVENT time (often future), not fetch time —
//!   `strategy_indicators::CalendarGate` suppresses trading around it.
//! - `sentiment` is always `None`: calendar events have no direction,
//!   only timing. Anything scoring them directionally is inventing data.
//! - Unknown impact strings map to [`Impact::High`] (fail-closed toward
//!   caution), malformed rows are skipped without killing the batch.
//! - Fetch failures degrade to `None` — a dead calendar never halts
//!   trading, exactly like the LLM cooldown.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use trading_core::{NewsFeed, NewsItem};

/// Weekly calendar JSON. Free, keyless, CORS-open.
pub const CALENDAR_URL: &str = "https://nfs.faireconomy.media/ff_calendar_thisweek.json";
/// Re-fetch at most hourly; the calendar barely moves intraday.
const REFRESH_SECS: u64 = 3600;
/// Emit events up to this far in the past (catch-up after downtime).
const BACKFILL_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Impact {
    Low,
    Medium,
    High,
    Holiday,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CalendarEvent {
    pub country: String,
    pub title: String,
    pub date: DateTime<Utc>,
    pub impact: Impact,
    pub forecast: String,
    pub previous: String,
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(default)]
    country: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    date: String,
    #[serde(default)]
    impact: String,
    #[serde(default)]
    forecast: String,
    #[serde(default)]
    previous: String,
}

pub fn parse_impact(s: &str) -> Impact {
    match s.trim().to_lowercase().as_str() {
        "low" => Impact::Low,
        "medium" => Impact::Medium,
        "high" => Impact::High,
        "holiday" => Impact::Holiday,
        // Unknown strings fail CLOSED toward caution (see module docs).
        _ => Impact::High,
    }
}

fn parse_row(raw: &RawEvent) -> Option<CalendarEvent> {
    if raw.country.is_empty() || raw.title.is_empty() {
        return None;
    }
    let date = DateTime::parse_from_rfc3339(&raw.date).ok()?.with_timezone(&Utc);
    Some(CalendarEvent {
        country: raw.country.clone(),
        title: raw.title.clone(),
        date,
        impact: parse_impact(&raw.impact),
        forecast: raw.forecast.clone(),
        previous: raw.previous.clone(),
    })
}

/// Parse a calendar JSON array into events, sorted by date. Bad rows are
/// skipped; an unparseable body yields an empty vec — never an error that
/// could halt a trading loop.
pub fn parse_calendar(body: &serde_json::Value) -> Vec<CalendarEvent> {
    let mut out: Vec<CalendarEvent> = body
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<RawEvent>(v.clone()).ok())
                .filter_map(|r| parse_row(&r))
                .collect()
        })
        .unwrap_or_default();
    out.sort_by_key(|e| e.date);
    out
}

pub fn event_headline(e: &CalendarEvent) -> String {
    let impact = match e.impact {
        Impact::Low => "Low",
        Impact::Medium => "Medium",
        Impact::High => "High",
        Impact::Holiday => "Holiday",
    };
    format!(
        "[{}] {} ({impact} impact; forecast: {}; previous: {})",
        e.country,
        e.title,
        if e.forecast.is_empty() { "—" } else { &e.forecast },
        if e.previous.is_empty() { "—" } else { &e.previous },
    )
}

pub struct CalendarFeed {
    url: String,
    min_impact: Impact,
    client: reqwest::Client,
    events: Vec<CalendarEvent>,
    index: usize,
    /// Already-emitted (date, country, title) keys: refreshes never
    /// re-emit, and consumers never see duplicates across refetch.
    emitted: std::collections::HashSet<(DateTime<Utc>, String, String)>,
    fetched_at: Option<std::time::Instant>,
}

impl CalendarFeed {
    pub fn new() -> Self {
        Self::with_url(CALENDAR_URL)
    }

    pub fn with_url(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            min_impact: Impact::High,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to build HTTP client"),
            events: Vec::new(),
            index: 0,
            emitted: std::collections::HashSet::new(),
            fetched_at: None,
        }
    }

    pub fn with_min_impact(mut self, impact: Impact) -> Self {
        self.min_impact = impact;
        self
    }

    fn stale(&self) -> bool {
        match self.fetched_at {
            None => true,
            Some(t) => t.elapsed().as_secs() >= REFRESH_SECS,
        }
    }

    async fn refresh(&mut self) {
        let fetched = self.fetch_events().await;
        if !fetched.is_empty() {
            self.events = fetched;
            self.index = 0;
            let cutoff = Utc::now() - chrono::Duration::seconds(BACKFILL_SECS);
            self.emitted.retain(|(d, _, _)| *d >= cutoff);
        }
        // Always stamped, even on failure: a dead endpoint backs off for
        // REFRESH_SECS instead of being hammered every tick.
        self.fetched_at = Some(std::time::Instant::now());
    }

    /// One-shot fetch of the whole week as structured events (for gates
    /// that need country/impact/timing, not the headline stream).
    /// Failure → empty vec. Applies the feed's impact filter + backfill.
    pub async fn fetch_events(&self) -> Vec<CalendarEvent> {
        let body: Option<serde_json::Value> = async {
            let resp = self.client.get(&self.url).send().await.ok()?;
            if !resp.status().is_success() {
                return None;
            }
            resp.json().await.ok()
        }
        .await;
        let mut events = body.map(|b| parse_calendar(&b)).unwrap_or_default();
        let cutoff = Utc::now() - chrono::Duration::seconds(BACKFILL_SECS);
        events.retain(|e| e.impact >= self.min_impact && e.date >= cutoff);
        events
    }
}

impl Default for CalendarFeed {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NewsFeed for CalendarFeed {
    async fn next_headline(&mut self) -> Option<NewsItem> {
        if self.stale() {
            self.refresh().await;
        }
        loop {
            let e = self.events.get(self.index)?;
            self.index += 1;
            let key = (e.date, e.country.clone(), e.title.clone());
            if self.emitted.insert(key) {
                return Some(NewsItem { time: e.date, headline: event_headline(e), sentiment: None });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> serde_json::Value {
        serde_json::json!([
            {"country": "USD", "title": "CPI m/m", "date": "2026-09-16T12:30:00-04:00",
             "impact": "High", "forecast": "0.3%", "previous": "0.2%"},
            {"country": "EUR", "title": "Random speech", "date": "2026-09-15T10:00:00+02:00",
             "impact": "Low", "forecast": "", "previous": ""},
            {"country": "GBP", "title": "Bank Holiday", "date": "2026-09-17T00:00:00+01:00",
             "impact": "Holiday", "forecast": "", "previous": ""},
            {"country": "", "title": "No country", "date": "2026-09-18T00:00:00Z",
             "impact": "High", "forecast": "", "previous": ""},
            {"country": "USD", "title": "Bad date", "date": "not-a-date",
             "impact": "High", "forecast": "", "previous": ""},
            {"country": "JPY", "title": "Mystery impact", "date": "2026-09-18T12:00:00+09:00",
             "impact": "Cataclysmic", "forecast": "", "previous": ""}
        ])
    }

    #[test]
    fn parses_sorted_skips_bad_maps_unknown_high() {
        let events = parse_calendar(&sample());
        // 4 valid: USD CPI, EUR speech, GBP holiday, JPY mystery(=High).
        assert_eq!(events.len(), 4);
        assert!(events.windows(2).all(|w| w[0].date <= w[1].date));
        assert_eq!(events[0].country, "EUR"); // earliest first
        let jpy = events.iter().find(|e| e.country == "JPY").unwrap();
        assert_eq!(jpy.impact, Impact::High); // unknown fails closed
        let usd = events.iter().find(|e| e.country == "USD").unwrap();
        assert_eq!(
            usd.date,
            DateTime::parse_from_rfc3339("2026-09-16T12:30:00-04:00").unwrap().with_timezone(&Utc)
        );
    }

    #[test]
    fn headline_carries_impact_forecast_previous() {
        let events = parse_calendar(&sample());
        let usd = events.iter().find(|e| e.country == "USD").unwrap();
        let h = event_headline(usd);
        assert!(h.contains("[USD]") && h.contains("CPI m/m") && h.contains("High"));
        assert!(h.contains("0.3%") && h.contains("0.2%"));
    }

    #[test]
    fn impact_ordering_filters() {
        assert!(Impact::Low < Impact::Medium && Impact::Medium < Impact::High);
        assert_eq!(parse_impact("high"), Impact::High);
        assert_eq!(parse_impact("  Medium "), Impact::Medium);
        assert_eq!(parse_impact("???"), Impact::High);
    }

    #[test]
    fn garbage_body_parses_to_empty_not_error() {
        assert!(parse_calendar(&serde_json::json!({})).is_empty());
        assert!(parse_calendar(&serde_json::json!([])).is_empty());
        assert!(parse_calendar(&serde_json::json!("nope")).is_empty());
    }

    /// Unreachable host must degrade to None quickly, never hang or panic.
    #[tokio::test]
    async fn dead_endpoint_degrades_to_silence() {
        let mut feed = CalendarFeed::with_url("http://127.0.0.1:9/nope");
        assert!(feed.next_headline().await.is_none());
        assert!(feed.next_headline().await.is_none());
    }

    /// Real weekly fetch. Ignored by default (needs network, read-only).
    /// Run: cargo test -p news-calendar -- --ignored
    #[tokio::test]
    #[ignore]
    async fn live_weekly_fetch_yields_events() {
        let mut feed = CalendarFeed::new();
        let first = feed.next_headline().await.expect("expected calendar events this week");
        assert!(first.headline.starts_with('['));
        assert_eq!(first.sentiment, None);
    }
}
