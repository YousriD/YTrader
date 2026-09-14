//! OANDA v20 adapter (P2-4): the first REAL `Broker` implementation.
//!
//! Practice-host ONLY by construction — [`is_practice_url`] gates the
//! orchestrator, and the real-money host is refused mechanically, not
//! by convention. Demo flow: practice account → token in
//! `OANDA_API_KEY` → run `mode = "live"` → `analytics gate` on the log.
//!
//! Accounting model (read carefully): OANDA positions live at
//! ACCOUNT+instrument level, not per agent. Each `OandaBroker` mirrors
//! one instrument of one account: `reconcile()` overwrites the mirror
//! from summary + openPositions + pricing; `place_order()` updates the
//! mirror locally from the fill response. `account_state()` therefore
//! reflects last-reconciled truth plus local fills — same staleness
//! model as any REST adapter. Multi-agent sharing one account+symbol
//! merges positions: use distinct symbols per agent (enforced) or
//! separate sub-accounts per agent (documented follow-up).
//!
//! [`Broker::withdraw`] is NOT supported (v20 has no withdrawal
//! endpoint — money moves via OANDA's site). Splits against this broker
//! defer with a rejection notice via the existing agent logic.

use std::fmt;

use async_trait::async_trait;
use chrono::Utc;
use trading_core::{AccountState, Broker, BrokerError, Fill, Order, Side};

/// OANDA practice host. The ONLY host live mode accepts.
pub const PRACTICE_HOST: &str = "api-fxpractice.oanda.com";
/// OANDA real-money host. Refused mechanically — never allowlisted.
pub const TRADE_HOST: &str = "api-fxtrade.oanda.com";

/// True only for https URLs on the practice host. Everything else —
/// real-money host, http, lookalikes (`api-fxpractice.oanda.com.evil.io`),
/// garbage — is refused.
pub fn is_practice_url(url: &str) -> bool {
    let url = url.trim().trim_end_matches('/');
    let rest = match url.strip_prefix("https://") {
        Some(r) => r,
        None => return false,
    };
    let host = rest.split('/').next().unwrap_or("");
    host.eq_ignore_ascii_case(PRACTICE_HOST)
}

/// Signed OANDA unit string: buys positive, sells negative.
pub fn oanda_units(side: Side, units: f64) -> String {
    match side {
        Side::Buy => format!("{units}"),
        Side::Sell => format!("-{units}"),
    }
}

pub struct OandaBroker {
    base_url: String,
    api_key: String, // never logged (manual Debug impl below)
    account_id: String,
    symbol: String, // "EUR_USD" — the ONE instrument this mirror tracks
    client: reqwest::Client,
    balance: f64,
    open_units: f64, // signed: +long / -short
    avg_entry: f64,
    last_price: Option<f64>,
}

impl fmt::Debug for OandaBroker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OandaBroker")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("account_id", &self.account_id)
            .field("symbol", &self.symbol)
            .field("balance", &self.balance)
            .field("open_units", &self.open_units)
            .field("avg_entry", &self.avg_entry)
            .field("last_price", &self.last_price)
            .finish()
    }
}

impl OandaBroker {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, account_id: impl Into<String>, symbol: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            account_id: account_id.into(),
            symbol: symbol.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to build HTTP client"),
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
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .map_err(|e| BrokerError::Other(format!("oanda GET {path} failed: {e}")))?;
        let status = resp.status();
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BrokerError::Other(format!("oanda GET {path}: bad body: {e}")))?;
        if !status.is_success() {
            return Err(map_api_error(&body));
        }
        Ok(body)
    }

    /// Apply one fill to the local mirror (same average-cost rules as
    /// PaperBroker: add averages in, closes realize, flips reprice).
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

/// Parse a 201 order response into (fill price, filled units).
/// Missing/invalid fill data is an explicit error, never a guess.
pub fn parse_fill(body: &serde_json::Value) -> Result<(f64, f64), BrokerError> {
    let tx = body.get("orderFillTransaction").ok_or_else(|| map_api_error(body))?;
    let price: f64 = tx
        .get("price")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| BrokerError::Other("oanda fill without price".to_string()))?;
    let units: f64 = tx
        .get("units")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| BrokerError::Other("oanda fill without units".to_string()))?;
    Ok((price, units.abs()))
}

/// Map an OANDA error body to `BrokerError`. Margin problems become
/// `InsufficientBalance` (the agent layer understands that one);
/// everything else is an explicit `Other`.
pub fn map_api_error(body: &serde_json::Value) -> BrokerError {
    let code = body
        .get("errorCode")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("orderCancelTransaction").and_then(|t| t.get("reason")).and_then(|v| v.as_str()))
        .unwrap_or("");
    let msg = body
        .get("errorMessage")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("orderCancelTransaction").and_then(|t| t.get("reason")).and_then(|v| v.as_str()))
        .unwrap_or("unknown oanda error");
    if code.contains("MARGIN") || code.contains("INSUFFICIENT") {
        return BrokerError::InsufficientBalance;
    }
    BrokerError::Other(format!("oanda: {msg}"))
}

/// Parse GET-summary body into account balance.
pub fn parse_balance(body: &serde_json::Value) -> Result<f64, BrokerError> {
    body.get("account")
        .and_then(|a| a.get("balance"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| BrokerError::Other("oanda summary without balance".to_string()))
}

/// Parse GET-openPositions body into net (units, avg) for `symbol`.
/// Hedged (both sides open) accounts are refused loudly — netting them
/// would silently misstate the position the agent trades against.
pub fn parse_position(body: &serde_json::Value, symbol: &str) -> Result<(f64, f64), BrokerError> {
    let positions = body
        .get("positions")
        .and_then(|v| v.as_array())
        .ok_or_else(|| BrokerError::Other("oanda positions without list".to_string()))?;
    let pos = positions.iter().find(|p| p.get("instrument").and_then(|v| v.as_str()) == Some(symbol));
    let pos = match pos {
        Some(p) => p,
        None => return Ok((0.0, 0.0)),
    };
    let side_units = |side: &str| -> (f64, f64) {
        let units: f64 = pos
            .get(side)
            .and_then(|s| s.get("units"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        let avg: f64 = pos
            .get(side)
            .and_then(|s| s.get("averagePrice"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        (units, avg)
    };
    let (long_units, long_avg) = side_units("long");
    let (short_units, short_avg) = side_units("short");
    if long_units != 0.0 && short_units != 0.0 {
        return Err(BrokerError::Other(format!(
            "oanda {symbol} is hedged (long {long_units} + short {short_units}) — one side per instrument only"
        )));
    }
    if long_units != 0.0 {
        Ok((long_units, long_avg))
    } else if short_units != 0.0 {
        Ok((-short_units, short_avg))
    } else {
        Ok((0.0, 0.0))
    }
}

/// Parse GET-pricing body into the mid of the first price entry.
pub fn parse_mid(body: &serde_json::Value) -> Option<f64> {
    let price = body.get("prices")?.as_array()?.first()?;
    let bid: f64 = price.get("bids")?.as_array()?.first()?.get("price")?.as_str()?.parse().ok()?;
    let ask: f64 = price.get("asks")?.as_array()?.first()?.get("price")?.as_str()?.parse().ok()?;
    Some((bid + ask) / 2.0)
}

#[async_trait]
impl Broker for OandaBroker {
    async fn place_order(&mut self, order: Order) -> Result<Fill, BrokerError> {
        if !order.units.is_finite() || order.units <= 0.0 {
            return Err(BrokerError::Other("order units must be positive".to_string()));
        }
        let body = serde_json::json!({
            "order": {
                "type": "MARKET",
                "instrument": order.symbol,
                "units": oanda_units(order.side, order.units),
                "positionFill": "DEFAULT",
            }
        });
        let resp = self
            .client
            .post(self.url(&format!("/v3/accounts/{}/orders", self.account_id)))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| BrokerError::Other(format!("oanda order failed: {e}")))?;
        let status = resp.status();
        let resp_body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| BrokerError::Other(format!("oanda order: bad body: {e}")))?;
        if !status.is_success() {
            return Err(map_api_error(&resp_body));
        }
        let (price, _filled) = parse_fill(&resp_body)?;
        self.apply_fill(order.side, order.units, price);
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
        // Reference mark between reconciles. In production this should be
        // venue pricing (future feed-oanda); the mock feed's marks are for
        // practice runs only — reconcile() re-anchors to venue truth.
        self.last_price = Some(price);
    }

    async fn reconcile(&mut self) -> Result<(), BrokerError> {
        let summary = self.get(&format!("/v3/accounts/{}/summary", self.account_id)).await?;
        self.balance = parse_balance(&summary)?;
        let positions = self.get(&format!("/v3/accounts/{}/openPositions", self.account_id)).await?;
        let (units, avg) = parse_position(&positions, &self.symbol)?;
        self.open_units = units;
        self.avg_entry = avg;
        // Best-effort venue mark: summary+positions are the critical
        // truth; a pricing failure keeps the previous mark, never fails
        // the whole reconcile.
        if let Ok(pricing) = self.get(&format!("/v3/accounts/{}/pricing?instruments={}", self.account_id, self.symbol)).await {
            if let Some(mid) = parse_mid(&pricing) {
                self.last_price = Some(mid);
            }
        }
        Ok(())
    }

    fn withdraw(&mut self, _amount: f64) -> Result<(), BrokerError> {
        Err(BrokerError::Other(
            "oanda has no withdrawal endpoint — move funds via OANDA's site; splits defer until then".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn practice_url_gate() {
        assert!(is_practice_url("https://api-fxpractice.oanda.com"));
        assert!(is_practice_url("https://api-fxpractice.oanda.com/"));
        assert!(is_practice_url("https://api-fxpractice.oanda.com/v3/accounts"));
        assert!(!is_practice_url("https://api-fxtrade.oanda.com"));
        assert!(!is_practice_url("http://api-fxpractice.oanda.com")); // https only
        assert!(!is_practice_url("https://api-fxpractice.oanda.com.evil.io"));
        assert!(!is_practice_url("https://evil.io/api-fxpractice.oanda.com"));
        assert!(!is_practice_url("not a url"));
        assert!(!is_practice_url(""));
    }

    #[test]
    fn units_sign_mapping() {
        assert_eq!(oanda_units(Side::Buy, 100.0), "100");
        assert_eq!(oanda_units(Side::Sell, 100.0), "-100");
        assert_eq!(oanda_units(Side::Sell, 1.5), "-1.5");
    }

    #[test]
    fn parses_market_fill() {
        let body = serde_json::json!({
            "orderCreateTransaction": { "id": "1", "type": "MARKET_ORDER_CREATE" },
            "orderFillTransaction": {
                "id": "2", "type": "MARKET_ORDER", "instrument": "EUR_USD",
                "units": "100", "price": "1.10012",
            },
            "lastTransactionID": "2",
        });
        assert_eq!(parse_fill(&body).unwrap(), (1.10012, 100.0));
        let cancel = serde_json::json!({
            "orderCancelTransaction": { "type": "ORDER_CANCEL", "reason": "INSUFFICIENT_MARGIN" },
        });
        assert!(matches!(parse_fill(&cancel).unwrap_err(), BrokerError::InsufficientBalance));
    }

    #[test]
    fn maps_margin_errors_to_insufficient_balance() {
        let margin = serde_json::json!({
            "errorCode": "INSUFFICIENT_MARGIN",
            "errorMessage": "Insufficient margin to create order",
        });
        assert!(matches!(map_api_error(&margin), BrokerError::InsufficientBalance));
        let other = serde_json::json!({
            "errorCode": "INVALID_INSTRUMENT",
            "errorMessage": "No such instrument",
        });
        assert!(matches!(map_api_error(&other), BrokerError::Other(_)));
    }

    #[test]
    fn parses_summary_balance() {
        let body = serde_json::json!({ "account": { "id": "1", "balance": "1234.56" } });
        assert!((parse_balance(&body).unwrap() - 1234.56).abs() < 1e-9);
        assert!(parse_balance(&serde_json::json!({})).is_err());
    }

    #[test]
    fn parses_positions_long_short_flat_and_rejects_hedge() {
        let mk = |long: &str, long_px: &str, short: &str, short_px: &str| {
            serde_json::json!({ "positions": [{
                "instrument": "EUR_USD",
                "long": { "units": long, "averagePrice": long_px },
                "short": { "units": short, "averagePrice": short_px },
            }]})
        };
        assert_eq!(parse_position(&mk("100", "1.10", "0", "0.0"), "EUR_USD").unwrap(), (100.0, 1.10));
        assert_eq!(parse_position(&mk("0", "0.0", "50", "1.20"), "EUR_USD").unwrap(), (-50.0, 1.20));
        assert_eq!(parse_position(&serde_json::json!({ "positions": [] }), "EUR_USD").unwrap(), (0.0, 0.0));
        assert!(parse_position(&mk("10", "1.10", "5", "1.20"), "EUR_USD").is_err());
    }

    #[test]
    fn parses_pricing_mid() {
        let body = serde_json::json!({ "prices": [{
            "instrument": "EUR_USD",
            "bids": [{ "price": "1.10000" }],
            "asks": [{ "price": "1.10020" }],
        }]});
        assert!((parse_mid(&body).unwrap() - 1.10010).abs() < 1e-9);
        assert_eq!(parse_mid(&serde_json::json!({})), None);
    }

    #[test]
    fn mirror_math_add_close_flip() {
        let mut b = OandaBroker::new("https://api-fxpractice.oanda.com", "k", "1", "EUR_USD");
        b.apply_fill(Side::Buy, 10.0, 1.0);
        b.apply_fill(Side::Buy, 10.0, 3.0); // avg 2.0
        b.apply_fill(Side::Sell, 5.0, 4.0); // +10, keeps avg
        let st = b.account_state();
        assert_eq!(st.open_units, 15.0);
        assert!((st.entry_price.unwrap() - 2.0).abs() < 1e-9);
        assert!((st.balance - 10.0).abs() < 1e-9);
        b.apply_fill(Side::Sell, 30.0, 3.0); // close 15 (+15), short 15 @3.0
        let st = b.account_state();
        assert_eq!(st.open_units, -15.0);
        assert!((st.entry_price.unwrap() - 3.0).abs() < 1e-9);
        assert!((st.balance - 25.0).abs() < 1e-9);
    }

    /// Read-only practice check. Ignored by default: needs a practice
    /// token, never places orders. Run with:
    /// OANDA_API_KEY=... OANDA_ACCOUNT_ID=... cargo test -p broker-oanda -- --ignored
    #[tokio::test]
    #[ignore]
    async fn practice_reconcile_read_only() {
        let key = std::env::var("OANDA_API_KEY").expect("OANDA_API_KEY required");
        let account = std::env::var("OANDA_ACCOUNT_ID").expect("OANDA_ACCOUNT_ID required");
        let mut b = OandaBroker::new("https://api-fxpractice.oanda.com", key, account, "EUR_USD");
        b.reconcile().await.expect("practice reconcile must succeed");
        let st = b.account_state();
        assert!(st.balance.is_finite() && st.equity.is_finite());
    }
}
