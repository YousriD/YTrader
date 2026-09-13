use serde::Deserialize;
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
    /// LLM-driven, always wrapped in a Hybrid with an SMA fallback.
    /// Requires ANTHROPIC_API_KEY at runtime, else the agent is skipped.
    Llm { persona: String, fallback_fast: usize, fallback_slow: usize },
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
    pub strategy: StrategySpec,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunConfig {
    /// "test" = paper trading only, always safe.
    /// "live" = refused by the orchestrator until a real broker adapter
    /// exists (see docs/PLAN.md:P0-3). No silent paper-as-live, ever.
    pub mode: Mode,
    pub ticks: u32,
    pub tick_delay_ms: u64,
    pub news_every_n_ticks: u32,
    /// Persist a per-agent `snapshot` record every N ticks so a restart
    /// with `--resume` can rebuild broker/baseline state. Crash window =
    /// up to N ticks of history. Defaults to 50; set 0 to disable.
    #[serde(default = "default_snapshot_every")]
    pub snapshot_every_n_ticks: u32,
    pub agents: Vec<AgentSpec>,
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
