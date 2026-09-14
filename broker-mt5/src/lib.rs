//! MT5 bridge adapter (P2-7): trade an MT5 demo through a LOCAL bridge.
//!
//! Shape: `yTrader (Rust) → http://127.0.0.1:PORT (bridge-mt5/) → MT5
//! terminal → broker server`. Your MT credentials live ONLY in the
//! terminal — this crate never sees, stores, or transmits them. The
//! only secret-adjacent config is the bridge URL (localhost).
//!
//! Demo-only is mechanical: [`reconcile()`] reads the account's
//! `trade_mode` from the bridge and refuses anything but `DEMO`.
//! Units convert to MT lots via the symbol's `contract_size`
//! (`lots = units / contract_size`, rounded DOWN to `volume_step`);
//! fills convert back, so the mirror stays in our units. Risk, splits,
//! and analytics are untouched — same `Broker` contract as every venue.

use async_trait::async_trait;
use chrono::Utc;
use trading_core::{AccountState, Broker, BrokerError, Fill, Order, Side};

/// Magic number tagging every order this adapter places (visible in the
/// terminal + bridge logs for attribution).
pub const YTRADER_MAGIC: u32 = 20260914;

/// Map our `BASE_QUOTE` symbol to an MT symbol, unless overridden.
/// `EUR_USD` → `EURUSD`. Override passes through untouched.
pub fn mt5_symbol(symbol: &str, venue_override: Option<&str>) -> String {
    match venue_override {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => symbol.replace('_', "").replace('/', "").to_uppercase(),
    }
}

/// True only for plain-HTTP localhost bridge URLs. Everything else —
/// https lookalikes, remote hosts (orders would cross the network in
/// plaintext), garbage — is refused by the live gate.
pub fn is_local_bridge_url(url: &str) -> bool {
    let url = url.trim().trim_end_matches('/');
    let rest = match url.strip_prefix("http://") {
        Some(r) => r,
        None => return false,
    };
    let host = rest.split(['/', ':']).next().unwrap_or("");
    host.eq_ignore_ascii_case("127.0.0.1") || host.eq_ignore_ascii_case("localhost")
}

#[derive(Debug)]
pub struct Mt5Broker {
    base_url: String,
    symbol: String,
    agent_tag: String,
    client: reqwest::Client,
    contract_size: f64,
    volume_min: f64,
    volume_step: f64,
    balance: f64,
    open_units: f64, // signed, OUR units: +long / -short
    avg_entry: f64,
    last_price: Option<f64>,
}

impl Mt5Broker {
    pub fn new(base_url: impl Into<String>, symbol: impl Into<String>, agent_tag: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            symbol: symbol.into(),
            agent_tag: agent_tag.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to build HTTP client"),
            contract_size: 100_000.0,
            volume_min: 0.01,
            volume_step: 0.01,
            balance: 0.0,
            open_units: 0.0,
            avg_entry: 0.0,
            last_price: None,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    async fn get(&self, path: &str) -> Result<serde_json::Value, BrokerError> {
        let resp = self
            .client
            .get(self.url(path))
            .send()
            .await
            .map_err(|e| BrokerError::Other(format!("mt5 bridge GET {path} failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(BrokerError::Other(format!("mt5 bridge GET {path}: HTTP {}", resp.status())));
        }
        resp.json().await.map_err(|e| BrokerError::Other(format!("mt5 bridge GET {path}: bad body: {e}")))
    }

    /// Convert our units to bridge lots, rounded DOWN to the volume step.
    /// Returns `Err` when even the minimum can't be afforded in steps.
    fn to_lots(&self, units: f64) -> Result<f64, BrokerError> {
        if self.contract_size <= 0.0 || self.volume_step <= 0.0 {
            return Err(BrokerError::Other("mt5: unknown contract spec (reconcile first)".to_string()));
        }
        let raw = units / self.contract_size;
        let lots = (raw / self.volume_step).floor() * self.volume_step;
        if lots < self.volume_min && (self.volume_min - lots) > 1e-9 {
            return Err(BrokerError::Other(format!(
                "below venue minimum: {units} units < {} lots",
                self.volume_min
            )));
        }
        Ok(lots.max(self.volume_min).min(raw))
    }

    /// Apply one fill to the local mirror (average-cost rules identical
    /// to PaperBroker: add averages in, closes realize, flips reprice).
    fn apply_fill(&mut self, side: Side, units: f64, price: f64) {
        let signed = match side {
            Side::Buy => units,
            Side::Sell => -units,
        };
        if self.open_units == 0.0 {
            self.avg_entry = price;
        } else if self.open_units.signum() == signed.signum() {
            let total = self.open_units + signed;
            self.avg_entry = (self.avg_entry * self.open_units + price * signed) / total;
        } else {
            let closing = signed.abs().min(self.open_units.abs());
            let per_unit = if self.open_units > 0.0 { price - self.avg_entry } else { self.avg_entry - price };
            self.balance += per_unit * closing;
            if self.open_units + signed != 0.0
                && (self.open_units + signed).signum() != self.open_units.signum()
            {
                self.avg_entry = price;
            }
        }
        self.open_units += signed;
        if self.open_units == 0.0 {
            self.avg_entry = 0.0;
        }
    }
}

/// Parse GET /health. Returns `(connected, trade_mode)`.
fn parse_health(body: &serde_json::Value) -> Result<(bool, String), BrokerError> {
    let connected = body.get("connected").and_then(|v| v.as_bool()).ok_or_else(|| {
        BrokerError::Other("mt5 bridge /health without `connected`".to_string())
    })?;
    let mode = body
        .get("trade_mode")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    Ok((connected, mode))
}

#[async_trait]
impl Broker for Mt5Broker {
    async fn place_order(&mut self, order: Order) -> Result<Fill, BrokerError> {
        if !order.units.is_finite() || order.units <= 0.0 {
            return Err(BrokerError::Other("order units must be positive".to_string()));
        }
        let lots = self.to_lots(order.units)?;
        let body = serde_json::json!({
            "symbol": self.symbol,
            "side": format!("{:?}", order.side).to_uppercase(),
            "volume_lots": lots,
            "comment": format!("ytrader/{}", self.agent_tag),
            "magic": YTRADER_MAGIC,
        });
        let resp: serde_json::Value = self
            .client
            .post(self.url("/order"))
            .json(&body)
            .send()
            .await
            .map_err(|e| BrokerError::Other(format!("mt5 bridge POST /order failed: {e}")))?
            .json()
            .await
            .map_err(|e| BrokerError::Other(format!("mt5 bridge /order: bad body: {e}")))?;
        if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
            return Err(map_bridge_error(err));
        }
        let price: f64 = resp
            .get("price")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| BrokerError::Other("mt5 bridge /order without price".to_string()))?;
        let filled_lots: f64 = resp.get("volume").and_then(|v| v.as_f64()).unwrap_or(lots);
        let filled_units = filled_lots * self.contract_size;
        self.apply_fill(order.side, filled_units, price);
        Ok(Fill { order, price, time: Utc::now() })
    }

    fn account_state(&self) -> AccountState {
        let unrealized = match self.last_price {
            Some(p) if self.open_units != 0.0 => {
                let per_unit = if self.open_units > 0.0 { p - self.avg_entry } else { self.avg_entry - p };
                per_unit * self.open_units.abs()
            }
            _ => 0.0,
        };
        AccountState {
            balance: self.balance,
            equity: self.balance + unrealized,
            open_units: self.open_units,
            entry_price: if self.open_units != 0.0 { Some(self.avg_entry) } else { None },
        }
    }

    fn last_price(&self) -> Option<f64> {
        self.last_price
    }

    fn mark_price(&mut self, price: f64) {
        // Reference mark between reconciles. Production truth is venue
        // pricing; the mock feed's marks are for practice runs only.
        self.last_price = Some(price);
    }

    async fn reconcile(&mut self) -> Result<(), BrokerError> {
        // 1. Terminal alive + DEMO mode. Anything else fails closed.
        let health = self.get("/health").await?;
        let (connected, mode) = parse_health(&health)?;
        if !connected {
            return Err(BrokerError::Other("mt5 bridge: terminal not connected".to_string()));
        }
        if mode != "DEMO" {
            return Err(BrokerError::Other(format!(
                "mt5 bridge: refusing non-demo account (trade_mode={mode})"
            )));
        }
        // 2. Contract spec first: conversions depend on it.
        let info = self.get(&format!("/symbol_info?symbol={}", self.symbol)).await?;
        self.contract_size =
            info.get("contract_size").and_then(|v| v.as_f64()).filter(|v| *v > 0.0).ok_or_else(|| {
                BrokerError::Other(format!("mt5 bridge: no contract_size for {}", self.symbol))
            })?;
        self.volume_min = info.get("volume_min").and_then(|v| v.as_f64()).unwrap_or(0.01);
        self.volume_step = info.get("volume_step").and_then(|v| v.as_f64()).unwrap_or(0.01);
        if info.get("trade_allowed").and_then(|v| v.as_bool()) == Some(false) {
            return Err(BrokerError::Other(format!("mt5 bridge: {} not tradeable", self.symbol)));
        }
        // 3. Account truth.
        let account = self.get("/account").await?;
        self.balance = account
            .get("balance")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| BrokerError::Other("mt5 bridge /account without balance".to_string()))?;
        // 4. Positions → net + VWAP for our symbol.
        let positions = self.get(&format!("/positions?symbol={}", self.symbol)).await?;
        let list = positions
            .get("positions")
            .and_then(|v| v.as_array())
            .ok_or_else(|| BrokerError::Other("mt5 bridge /positions without list".to_string()))?;
        let mut net_lots = 0.0;
        let mut px_vol = 0.0;
        for p in list {
            let vol: f64 = p.get("volume").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let px: f64 = p.get("price_open").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let signed = match p.get("type").and_then(|v| v.as_u64()) {
                Some(1) => -vol, // ORDER_TYPE_SELL
                _ => vol,        // ORDER_TYPE_BUY (default)
            };
            net_lots += signed;
            px_vol += px * signed.abs();
        }
        self.open_units = net_lots * self.contract_size;
        self.avg_entry = if net_lots.abs() > 0.0 { px_vol / net_lots.abs() } else { 0.0 };
        // 5. Best-effort venue mark (never fails the whole reconcile).
        if let Ok(px) = self.get(&format!("/price?symbol={}", self.symbol)).await {
            if let (Some(bid), Some(ask)) =
                (px.get("bid").and_then(|v| v.as_f64()), px.get("ask").and_then(|v| v.as_f64()))
            {
                self.last_price = Some((bid + ask) / 2.0);
            }
        }
        Ok(())
    }

    fn withdraw(&mut self, _amount: f64) -> Result<(), BrokerError> {
        Err(BrokerError::Other(
            "mt5 bridge has no withdrawal endpoint — splits defer until then".to_string(),
        ))
    }
}

/// Map a bridge `"error"` string to `BrokerError`. Margin-like failures
/// become `InsufficientBalance` (the agent layer understands that one).
fn map_bridge_error(err: &str) -> BrokerError {
    let up = err.to_uppercase();
    if up.contains("MARGIN") || up.contains("MONEY") || up.contains("VOLUME") && up.contains("MIN") {
        // careful: minimum-volume is a rulebook rejection, but surfacing
        // it as balance keeps agent behavior safe (hold + log).
        return BrokerError::InsufficientBalance;
    }
    BrokerError::Other(format!("mt5 bridge: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn symbol_mapping() {
        assert_eq!(mt5_symbol("EUR_USD", None), "EURUSD");
        assert_eq!(mt5_symbol("gbp/jpy", None), "GBPJPY");
        assert_eq!(mt5_symbol("EUR_USD", Some("EURUSD.a")), "EURUSD.a");
        assert_eq!(mt5_symbol("EUR_USD", Some("")), "EURUSD");
    }

    #[test]
    fn local_bridge_url_gate() {
        assert!(is_local_bridge_url("http://127.0.0.1:5001"));
        assert!(is_local_bridge_url("http://127.0.0.1:5001/"));
        assert!(is_local_bridge_url("http://localhost:5001"));
        assert!(!is_local_bridge_url("https://127.0.0.1:5001")); // http only
        assert!(!is_local_bridge_url("http://192.168.1.10:5001"));
        assert!(!is_local_bridge_url("http://example.com"));
        assert!(!is_local_bridge_url("http://127.0.0.1.evil.io"));
        assert!(!is_local_bridge_url("not a url"));
        assert!(!is_local_bridge_url(""));
    }

    /// Minimal canned HTTP server: `routes` maps path-prefix → (status, body).
    /// Returns base URL + hit counter. std-only reads, tokio runtime drives.
    fn mock_bridge(routes: Vec<(&'static str, u16, &'static str)>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(routes.len() * 4 + 8) {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut buf = [0u8; 4096];
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => break,
                };
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req.lines().next().unwrap_or("").split_whitespace().nth(1).unwrap_or("/").to_string();
                // Strip query string for route matching.
                let route_key = path.split('?').next().unwrap_or("/").to_string();
                hits_clone.fetch_add(1, Ordering::SeqCst);
                let (status, body) = routes
                    .iter()
                    .find(|(prefix, _, _)| route_key.starts_with(prefix))
                    .map(|(_, s, b)| (*s, *b))
                    .unwrap_or((404, r#"{"error":"no route"}"#));
                let reason = if status == 200 { "OK" } else { "Error" };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn healthy_routes() -> Vec<(&'static str, u16, &'static str)> {
        vec![
            ("/health", 200, r#"{"connected":true,"login":12345678,"server":"DEMO","trade_mode":"DEMO"}"#),
            (
                "/symbol_info",
                200,
                r#"{"contract_size":100000.0,"volume_min":0.01,"volume_max":100.0,"volume_step":0.01,"trade_allowed":true}"#,
            ),
            ("/account", 200, r#"{"balance":1000.0,"equity":1000.0,"currency":"USD","trade_allowed":true}"#),
            (
                "/positions",
                200,
                r#"{"positions":[{"ticket":11,"type":0,"volume":0.05,"price_open":1.1000},{"ticket":12,"type":0,"volume":0.05,"price_open":1.1200}]}"#,
            ),
            ("/price", 200, r#"{"bid":1.1100,"ask":1.1102}"#),
            ("/order", 200, r#"{"ticket":99,"price":1.1101,"volume":0.05}"#),
        ]
    }

    #[tokio::test]
    async fn reconcile_builds_mirror_from_bridge() {
        let (url, _) = mock_bridge(healthy_routes());
        let mut b = Mt5Broker::new(url, "EURUSD", "t");
        b.reconcile().await.unwrap();
        let st = b.account_state();
        assert!((st.balance - 1000.0).abs() < 1e-9);
        // 0.05 + 0.05 lots long = 10,000 units @ VWAP 1.11.
        assert!((st.open_units - 10_000.0).abs() < 1e-6);
        assert!((st.entry_price.unwrap() - 1.11).abs() < 1e-9);
        assert!((b.last_price.unwrap() - 1.1101).abs() < 1e-9);
    }

    #[tokio::test]
    async fn reconcile_refuses_non_demo_and_dead_terminal() {
        let (url, _) = mock_bridge(vec![(
            "/health",
            200,
            r#"{"connected":true,"trade_mode":"REAL"}"#,
        )]);
        let mut b = Mt5Broker::new(url, "EURUSD", "t");
        assert!(b.reconcile().await.is_err());

        let (url, _) = mock_bridge(vec![(
            "/health",
            200,
            r#"{"connected":false,"trade_mode":"DEMO"}"#,
        )]);
        let mut b = Mt5Broker::new(url, "EURUSD", "t");
        assert!(b.reconcile().await.is_err());
    }

    #[tokio::test]
    async fn place_order_converts_and_mirrors() {
        let (url, hits) = mock_bridge(healthy_routes());
        let mut b = Mt5Broker::new(url, "EURUSD", "t");
        b.reconcile().await.unwrap();
        let before = hits.load(Ordering::SeqCst);
        // 5,000 units = 0.05 lots; bridge fills 0.05 @1.1101 → adds.
        let fill = b
            .place_order(Order { symbol: "EURUSD".to_string(), side: Side::Buy, units: 5_000.0 })
            .await
            .unwrap();
        assert!((fill.price - 1.1101).abs() < 1e-9);
        assert!((b.account_state().open_units - 15_000.0).abs() < 1e-6);
        assert_eq!(hits.load(Ordering::SeqCst) - before, 1); // one POST
    }

    #[tokio::test]
    async fn below_minimum_rejected_without_http() {
        let (url, hits) = mock_bridge(healthy_routes());
        let mut b = Mt5Broker::new(url, "EURUSD", "t");
        b.reconcile().await.unwrap();
        let before = hits.load(Ordering::SeqCst);
        // 500 units = 0.005 lots < step-rounded minimum 0.01.
        assert!(b
            .place_order(Order { symbol: "EURUSD".to_string(), side: Side::Buy, units: 500.0 })
            .await
            .is_err());
        assert_eq!(hits.load(Ordering::SeqCst), before); // no HTTP fired
    }

    #[tokio::test]
    async fn bridge_error_maps_to_broker_error() {
        // Same routes, but /order reports a money error.
        let routes: Vec<(&str, u16, &str)> = healthy_routes()
            .into_iter()
            .map(|(p, s, b)| if p == "/order" { (p, s, r#"{"error":"Not enough money"}"#) } else { (p, s, b) })
            .collect();
        let (url, _) = mock_bridge(routes);
        let mut b = Mt5Broker::new(url, "EURUSD", "t");
        b.reconcile().await.unwrap();
        let err = b
            .place_order(Order { symbol: "EURUSD".to_string(), side: Side::Buy, units: 5_000.0 })
            .await
            .unwrap_err();
        assert!(matches!(err, BrokerError::InsufficientBalance));
    }

    #[test]
    fn withdraw_unsupported() {
        let mut b = Mt5Broker::new("http://127.0.0.1:9", "EURUSD", "t");
        assert!(b.withdraw(10.0).is_err());
    }
}
