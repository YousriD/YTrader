use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Test,
    Live,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StrategySpec {
    /// Pure algorithmic SMA crossover.
    Sma { fast: usize, slow: usize },
    /// Mean-reversion RSI on Wilder exits (buy leaving oversold, sell
    /// leaving overbought). Inventory-guarded like every registry entry.
    Rsi { period: usize, overbought: f64, oversold: f64 },
    /// Donchian channel breakout over the prior `channel` candles.
    Donchian { channel: usize },
    /// Volatility sizer around any inner strategy: keeps its side,
    /// scales size to `equity * risk_pct / atr` (see strategy-indicators).
    Atr { inner: Box<StrategySpec>, period: usize, risk_pct: f64, max_units: f64 },
    /// News gate around any inner strategy: suppresses entries for
    /// `cooldown_ticks` after news with `|sentiment| >= threshold`.
    NewsGated { inner: Box<StrategySpec>, cooldown_ticks: u32, sentiment_threshold: f64 },
    /// Calendar gate around any inner strategy: suppresses entries while
    /// a high-impact calendar event for either traded currency is within
    /// ±`window_minutes` of now. Currencies derive from the agent symbol.
    CalendarGated { inner: Box<StrategySpec>, window_minutes: u32 },
    /// Tier 2 regime router (P2-6): picks one tested candidate per tick.
    /// Works WITHOUT any key (deterministic rule brain); with `llm` set
    /// AND `ANTHROPIC_API_KEY` present it asks an LLM first and falls
    /// back to the rule brain. Unknown picks fall back to `default`.
    Router {
        candidates: HashMap<String, Box<StrategySpec>>,
        default: String,
        /// Candidate name for trending markets.
        trending: String,
        /// Candidate name for range markets.
        ranging: String,
        /// Lookback for the rule brain's drift/vol classifier.
        #[serde(default = "default_trend_window")]
        trend_window: usize,
        /// Present = LLM brain eligible (still needs the env key).
        #[serde(default)]
        llm: Option<RouterLlmSpec>,
    },
    /// LLM-driven, always wrapped in a Hybrid with an SMA fallback.
    /// Requires ANTHROPIC_API_KEY at runtime, else the agent is skipped.
    Llm { persona: String, fallback_fast: usize, fallback_slow: usize },
}

/// LLM-brain options for `StrategySpec::Router`. Key comes from
/// `ANTHROPIC_API_KEY` env, never from config.
#[derive(Debug, Clone, Deserialize)]
pub struct RouterLlmSpec {
    #[serde(default = "default_eval_every")]
    pub eval_every_n_ticks: u32,
}

fn default_trend_window() -> usize {
    20
}

fn default_eval_every() -> u32 {
    10
}

impl StrategySpec {
    /// Structural validation beyond parsing: every name a router maps
    /// must resolve to a built candidate (recursively). Called by the
    /// orchestrator; invalid agents are skipped loudly, never half-built.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            StrategySpec::Router { candidates, default, trending, ranging, .. } => {
                if !candidates.contains_key(default) {
                    return Err(format!("router default '{default}' is not a candidate"));
                }
                for (regime, name) in [("trending", trending), ("ranging", ranging)] {
                    if !candidates.contains_key(name) {
                        return Err(format!("router {regime} target '{name}' is not a candidate"));
                    }
                }
                for (name, sub) in candidates {
                    sub.validate().map_err(|e| format!("candidate '{name}': {e}"))?;
                }
                Ok(())
            }
            StrategySpec::Atr { inner, .. }
            | StrategySpec::NewsGated { inner, .. }
            | StrategySpec::CalendarGated { inner, .. } => inner.validate(),
            _ => Ok(()),
        }
    }

    /// True when this tree CANNOT run without an LLM key. Router trees
    /// degrade to the rule brain, so they never require one.
    pub fn requires_llm_key(&self) -> bool {
        match self {
            StrategySpec::Llm { .. } => true,
            StrategySpec::Atr { inner, .. }
            | StrategySpec::NewsGated { inner, .. }
            | StrategySpec::CalendarGated { inner, .. } => inner.requires_llm_key(),
            StrategySpec::Router { .. } => false,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentSpec {
    pub id: String,
    pub symbol: String,
    pub stake_min: f64,
    pub stake_max: f64,
    pub units: f64,
    /// e.g. 0.01 = 1%. Omit to disable.
    pub stop_loss_pct: Option<f64>,
    /// e.g. 0.02 = 2%. Omit to disable.
    pub take_profit_pct: Option<f64>,
    /// P1-2 inventory guard: when false (default), strategies skip new
    /// entries while a position is open instead of pyramiding.
    #[serde(default)]
    pub allow_pyramid: bool,
    /// Cap on emitted order size. Defaults to `units` when omitted.
    #[serde(default)]
    pub max_position_units: Option<f64>,
    /// P2-1 venue economics (all optional; omitted = current defaults).
    /// Smallest order the venue accepts (default 1.0 in the broker).
    #[serde(default)]
    pub min_units: Option<f64>,
    /// Smallest notional the venue accepts. None/0 = no floor.
    #[serde(default)]
    pub min_notional: Option<f64>,
    /// Cash deducted per filled unit, every fill (default 0.0).
    #[serde(default)]
    pub commission_per_unit: f64,
    /// Spread in pips (default 1.2 in the broker).
    #[serde(default)]
    pub spread_pips: Option<f64>,
    pub strategy: StrategySpec,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunConfig {
    /// "test" = paper trading only, always safe.
    /// "live" = real OANDA adapter, practice host ONLY (any other
    /// base_url keeps refusing — see `broker_oanda::is_practice_url`).
    pub mode: Mode,
    pub ticks: u32,
    pub tick_delay_ms: u64,
    pub news_every_n_ticks: u32,
    /// Persist a per-agent `snapshot` record every N ticks so a restart
    /// with `--resume` can rebuild broker/baseline state. Crash window =
    /// up to N ticks of history. Defaults to 50; set 0 to disable.
    #[serde(default = "default_snapshot_every")]
    pub snapshot_every_n_ticks: u32,
    /// Venue connection (P2-4). Only read in live mode; test mode
    /// ignores it entirely. API key NEVER lives here — env only.
    #[serde(default)]
    pub oanda: OandaConfig,
    pub agents: Vec<AgentSpec>,
}

/// OANDA connection. `account_id` is not secret; the token comes from
/// `OANDA_API_KEY` env at runtime so it can never be committed.
#[derive(Debug, Clone, Deserialize)]
pub struct OandaConfig {
    /// Default: the practice host. The real-money host is refused
    /// mechanically by the orchestrator.
    #[serde(default = "default_oanda_base_url")]
    pub base_url: String,
    /// Practice account id, e.g. "101-001-23456789-001".
    #[serde(default)]
    pub account_id: String,
}

impl Default for OandaConfig {
    fn default() -> Self {
        Self { base_url: default_oanda_base_url(), account_id: String::new() }
    }
}

fn default_oanda_base_url() -> String {
    "https://api-fxpractice.oanda.com".to_string()
}

fn default_snapshot_every() -> u32 {
    50
}

impl RunConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read config {:?}: {e}", path.as_ref()))?;
        toml::from_str(&text).map_err(|e| format!("failed to parse config: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_toml(strategy: &str, extra: &str) -> String {
        format!(
            r#"mode = "test"
ticks = 10
tick_delay_ms = 1
news_every_n_ticks = 15

[[agents]]
id = "a"
symbol = "EUR_USD"
stake_min = 10.0
stake_max = 100.0
units = 100.0
{extra}
strategy = {strategy}
"#
        )
    }

    #[test]
    fn parses_registry_and_nested_wrappers() {
        let cfg: RunConfig = toml::from_str(&agent_toml(
            r#"{ kind = "atr", inner = { kind = "news_gated", inner = { kind = "rsi", period = 14, overbought = 70.0, oversold = 30.0 }, cooldown_ticks = 5, sentiment_threshold = 0.5 }, period = 14, risk_pct = 0.02, max_units = 500.0 }"#,
            "",
        )).unwrap();
        assert_eq!(cfg.agents.len(), 1);
        // Inventory knobs default when omitted (constraint #7 compat).
        assert!(!cfg.agents[0].allow_pyramid);
        assert_eq!(cfg.agents[0].max_position_units, None);
        assert!(matches!(cfg.agents[0].strategy, StrategySpec::Atr { .. }));
    }

    #[test]
    fn old_configs_without_new_fields_still_parse() {
        let cfg: RunConfig = toml::from_str(&agent_toml(
            r#"{ kind = "sma", fast = 5, slow = 20 }"#,
            "",
        )).unwrap();
        assert!(matches!(cfg.agents[0].strategy, StrategySpec::Sma { fast: 5, slow: 20 }));
        assert_eq!(cfg.snapshot_every_n_ticks, 50); // serde default intact
    }

    #[test]
    fn parses_donchian_with_inventory_opt_in() {
        let cfg: RunConfig = toml::from_str(&agent_toml(
            r#"{ kind = "donchian", channel = 20 }"#,
            "allow_pyramid = true\nmax_position_units = 250.0",
        )).unwrap();
        assert!(cfg.agents[0].allow_pyramid);
        assert_eq!(cfg.agents[0].max_position_units, Some(250.0));
    }

    #[test]
    fn parses_calendar_gated_nesting() {
        let cfg: RunConfig = toml::from_str(&agent_toml(
            r#"{ kind = "calendar_gated", inner = { kind = "donchian", channel = 20 }, window_minutes = 30 }"#,
            "",
        )).unwrap();
        assert!(matches!(cfg.agents[0].strategy, StrategySpec::CalendarGated { window_minutes: 30, .. }));
    }

    #[test]
    fn parses_router_with_rule_brain_only() {
        // NOTE: TOML inline tables are single-line by spec.
        let cfg: RunConfig = toml::from_str(&agent_toml(
            r#"{ kind = "router", default = "mr", candidates = { mr = { kind = "rsi", period = 14, overbought = 70.0, oversold = 30.0 }, tr = { kind = "donchian", channel = 20 } }, trending = "tr", ranging = "mr" }"#,
            "",
        )).unwrap();
        let StrategySpec::Router { llm, trend_window, .. } = &cfg.agents[0].strategy else {
            panic!("expected router");
        };
        assert!(llm.is_none());
        assert_eq!(*trend_window, 20); // serde default
        assert!(!cfg.agents[0].strategy.requires_llm_key()); // runs keyless
        cfg.agents[0].strategy.validate().unwrap();
    }

    #[test]
    fn router_validation_rejects_dangling_names() {
        let bad = StrategySpec::Router {
            candidates: HashMap::from([("a".to_string(), Box::new(StrategySpec::Sma { fast: 5, slow: 20 }))]),
            default: "missing".to_string(),
            trending: "a".to_string(),
            ranging: "a".to_string(),
            trend_window: 20,
            llm: None,
        };
        assert!(bad.validate().is_err());
        assert!(!bad.requires_llm_key());
        let llm_nested = StrategySpec::Atr {
            inner: Box::new(StrategySpec::Llm {
                persona: "x".to_string(),
                fallback_fast: 5,
                fallback_slow: 20,
            }),
            period: 14,
            risk_pct: 0.02,
            max_units: 500.0,
        };
        assert!(llm_nested.requires_llm_key());
    }

    #[test]
    fn oanda_table_defaults_to_practice_and_parses_custom() {
        // Missing [oanda] entirely: practice URL, empty account.
        let cfg: RunConfig = toml::from_str(&agent_toml(
            r#"{ kind = "sma", fast = 5, slow = 20 }"#,
            "",
        )).unwrap();
        assert_eq!(cfg.oanda.base_url, "https://api-fxpractice.oanda.com");
        assert!(cfg.oanda.account_id.is_empty());
        // Explicit table respected.
        let text = agent_toml(r#"{ kind = "sma", fast = 5, slow = 20 }"#, "")
            + "\n[oanda]\nbase_url = \"https://api-fxtrade.oanda.com\"\naccount_id = \"001\"\n";
        let cfg: RunConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg.oanda.base_url, "https://api-fxtrade.oanda.com");
        assert_eq!(cfg.oanda.account_id, "001");
    }
}
