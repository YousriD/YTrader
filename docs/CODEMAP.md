# CODEMAP — TraderY — READ THIS FIRST

> For LLM agents: this is the file-to-behavior index. Start here, then open only the files listed for your task. Do not guess paths.

## 1. Workspace members (all in `C:\Repos\TraderY\`)

| Crate | Path | Role | Key exports |
|---|---|---|---|
| `core` | `core/src/lib.rs` | Trait contracts. No logic. | `Broker`, `MarketFeed`, `NewsFeed`, `Strategy`, `Order`, `Side`, `Fill`, `AccountState`, `Candle`, `NewsItem`, `MarketContext`, `RunMode`, `BrokerError` |
| `broker-paper` | `broker-paper/src/lib.rs` | Simulated fills. | `PaperBroker::new(balance)`, `spread()`, `place_order()`, `account_state()`, `mark_price()` |
| `feed-mock` | `feed-mock/src/lib.rs` | Random-walk prices. | `MockFeed::new(start_price, vol)`, `next_price()` |
| `news-mock` | `news-mock/src/lib.rs` | Fake headlines. | `MockNewsFeed::new(every_n_ticks)`, `next_headline()` |
| `strategy-sma` | `strategy-sma/src/lib.rs` | SMA crossover baseline. | `SmaCrossover::new(name, fast, slow, units)`, `decide()` |
| `strategy-llm` | `strategy-llm/src/lib.rs` | Claude-per-N-ticks. | `LlmStrategy::new(name, api_key, units, persona)`, `ask_claude()`, `decide()`, `is_healthy()` |
| `strategy-hybrid` | `strategy-hybrid/src/lib.rs` | LLM + SMA fallback. | `HybridStrategy::new(name, primary, fallback)`, `decide()` |
| `agent-runtime` | `agent-runtime/src/lib.rs` | Lifecycle + risk layer. | `Agent::new()`, `with_stop_loss()`, `with_take_profit()`, `reconcile()`, `check_risk_exits()`, `on_tick()`, `AgentEvent`, `AgentStatus` |
| `persistence` | `persistence/src/lib.rs` | JSONL append-only log. | `EventLog::open()`, `append()`, `read_all()`, `EventRecord` |
| `trading-config` | `trading-config/src/lib.rs` | `config.toml` parser. | `RunConfig::load()`, `Mode`, `AgentSpec`, `StrategySpec` |
| `licensing` | `licensing/src/lib.rs` | No-op seam. | `check()` -> `LicenseStatus::Unlicensed/Valid/Invalid` |
| `orchestrator` | `orchestrator/src/main.rs` | Binary, wiring only. | `main()`, `TickMsg`, `LogMsg`, `print_event()`, `persist_event()` |

Root files:
- `Cargo.toml` — workspace members + shared deps (tokio full, async-trait, rand 0.8, serde, serde_json, reqwest rustls-tls, chrono clock)
- `config.toml` — single run config. `mode`, `ticks`, `tick_delay_ms`, `news_every_n_ticks`, `[[agents]]`
- `README.md` — product story. Partially overclaims (see `docs/ANALYSIS.md`).
- `data/run-<timestamp>.jsonl` — runtime output. Created on each run. Never read back currently.

## 2. Runtime dataflow (exact order)

1. `orchestrator/src/main.rs:48-54` load `RunConfig` from `config.toml` (arg or default).
2. `orchestrator/src/main.rs:33-44` call `licensing::check()` — no-op today.
3. `orchestrator/src/main.rs:63-66` open `EventLog` at `data/run-<ts>.jsonl`.
4. `orchestrator/src/main.rs:72-105` for each `AgentSpec`: build `Strategy` (SMA direct, LLM wrapped in Hybrid), wrap in `PaperBroker::new(stake)`, wrap in `Agent::new()`, apply SL/TP, call `reconcile()` (no-op for paper).
5. `orchestrator/src/main.rs:113-156` spawn one tokio task per agent, subscribed to `broadcast::channel<TickMsg>(1024)`. Spawn one logger task on `mpsc` channel.
6. `orchestrator/src/main.rs:188-203` producer loop: `MockFeed::new(1.1000, 0.0006).next_price(config.agents[0].symbol)` + `MockNewsFeed::next_headline()`, broadcast same `TickMsg{candle, news}` to ALL agents.
7. Per agent per tick `agent-runtime/src/lib.rs:144-203` `on_tick(candle)`:
   - `broker.mark_price(candle.close)`, push history (cap 200)
   - emit `Tick{equity}`, death-check `equity<=0`
   - `check_risk_exits()` — if open position moved beyond SL/TP, force full close, skip strategy this tick
   - else `strategy.decide(ctx)` -> `broker.place_order(order)` -> `OrderPlaced` / `OrderRejected`
   - death-check again, split-check `equity >= baseline*2.0`
8. `orchestrator/src/main.rs:236-265` `persist_event()` maps each `AgentEvent` to `{kind, data}` JSON and appends. `Tick` events are persisted too (noisy).

## 3. Key function signatures to open first

- Agent tick: `agent-runtime/src/lib.rs:144` `pub async fn on_tick(&mut self, candle: Candle) -> Vec<AgentEvent>`
- Risk: `agent-runtime/src/lib.rs:109` `async fn check_risk_exits(&mut self) -> Option<AgentEvent>`
- Fill: `broker-paper/src/lib.rs:37` `async fn place_order(&mut self, order: Order) -> Result<Fill, BrokerError>`
- Equity: `broker-paper/src/lib.rs:86` `fn account_state(&self) -> AccountState`
- SMA: `strategy-sma/src/lib.rs:41` `async fn decide(&mut self, ctx: &MarketContext) -> Option<Order>`
- LLM: `strategy-llm/src/lib.rs:111` `decide()`, `strategy-llm/src/lib.rs:107` `is_healthy()`
- Hybrid: `strategy-hybrid/src/lib.rs:45` `decide()`
- Config: `trading-config/src/lib.rs:50` `RunConfig::load()`
- Log: `persistence/src/lib.rs:46` `append()`, `persistence/src/lib.rs:57` `read_all()` (currently unused by orchestrator)

## 4. Config shape (`config.toml`)

```toml
mode = "test" # "test" | "live" — live only skips llm agents today, still paper
ticks = 500
tick_delay_ms = 50
news_every_n_ticks = 15
[[agents]]
id = "algo-fast"
symbol = "EUR_USD"  # NOTE: producer only reads agents[0].symbol, rest ignored for feed
stake_min = 10.0
stake_max = 100.0
units = 1000.0      # fixed size, same for SL/TP close math
stop_loss_pct = 0.005
take_profit_pct = 0.01
strategy = { kind = "sma", fast = 5, slow = 20 }
# strategy = { kind = "llm", persona = "...", fallback_fast = 5, fallback_slow = 20 }
```

## 5. Gotchas for LLMs (do not re-discover)

- `G1` Single feed: all agents receive the identical `Candle` object per tick. No per-symbol feeds.
- `G2` Broadcast lag: `orchestrator/src/main.rs:144` `Lagged(_) => continue` silently skips ticks (hits LLM agents with 8s timeout first).
- `G3` Money is `f64` everywhere. No decimal type.
- `G4` No tests: zero `#[test]`, zero `tests/` dirs (verified 2026-09-13).
- `G5` Split does not debit broker; re-triggers every tick after double (see ANALYSIS A3).
- `G6` Shorts have no margin check; longs do (see ANALYSIS A4).
- `G7` Flip keeps stale `avg_entry_price` (see ANALYSIS A4).
- `G8` `read_all()` exists but is dead code at runtime — no replay/recovery.
