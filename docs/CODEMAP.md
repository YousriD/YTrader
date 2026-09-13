# CODEMAP — TraderY — READ THIS FIRST

> For LLM agents: this is the file-to-behavior index. Start here, then open only the files listed for your task. Do not guess paths.

## 1. Workspace members (all in `C:\Repos\TraderY\`)

| Crate | Path | Role | Key exports |
|---|---|---|---|
| `core` | `core/src/lib.rs` | Trait contracts. No logic. | `Broker` (+`withdraw` at :127, `reconcile` at :119), `MarketFeed`, `NewsFeed`, `Strategy`, `Order`, `Side`, `Fill`, `AccountState`, `Candle`, `NewsItem`, `MarketContext`, `RunMode`, `BrokerError` |
| `broker-paper` | `broker-paper/src/lib.rs` | Simulated fills. | `PaperBroker::new(balance)`, `restore()` (validates), `with_max_leverage()` (1x default), `spread()`, `place_order()` (:44), `account_state()`, `mark_price()`, `withdraw()`. Tests (9). |
| `feed-mock` | `feed-mock/src/lib.rs` | Random-walk prices. | `MockFeed::new(start_price, vol)`, `next_price()` |
| `news-mock` | `news-mock/src/lib.rs` | Fake headlines. | `MockNewsFeed::new(every_n_ticks)`, `next_headline()` |
| `strategy-sma` | `strategy-sma/src/lib.rs` | SMA crossover baseline. | `SmaCrossover::new(name, fast, slow, units)`, `decide()` |
| `strategy-llm` | `strategy-llm/src/lib.rs` | LLM every N ticks. | `LlmStrategy::new(name, api_key, units, persona)`, `ask_claude()`, `decide()`, `is_healthy()` |
| `strategy-hybrid` | `strategy-hybrid/src/lib.rs` | LLM + SMA fallback. | `HybridStrategy::new(name, primary, fallback)`, `decide()` |
| `agent-runtime` | `agent-runtime/src/lib.rs` | Lifecycle + risk layer. | `Agent::new()`, `Agent::restore()`, `baseline()`, `with_stop_loss()`, `with_take_profit()`, `reconcile()`, `check_risk_exits()` (:109), `on_tick()` (:144), split-withdraw at :194-215, `AgentEvent`, `AgentStatus`. Tests (7). |
| `persistence` | `persistence/src/lib.rs` | JSONL append-only log + resume fold. | `EventLog::open()`, `append()`, `EventLog::read_all()`, `latest_run_file()`, `load_snapshots()`, `Snapshot`. Tests (3). |
| `trading-config` | `trading-config/src/lib.rs` | `config.toml` parser. | `RunConfig::load()`, `Mode`, `AgentSpec`, `StrategySpec`, `snapshot_every_n_ticks` (default 50) |
| `licensing` | `licensing/src/lib.rs` | No-op seam. | `check()` -> `LicenseStatus::Unlicensed/Valid/Invalid` |
| `orchestrator` | `orchestrator/src/main.rs` | Binary, wiring only. | `main()`, `--resume` flag, `TickMsg`, `LogMsg` (Event/Snapshot/Final), `print_event()` |

Root files:
- `Cargo.toml` — workspace members + shared deps (tokio full, async-trait, rand 0.8, serde, serde_json, reqwest rustls-tls, chrono clock+serde)
- `config.toml` — single run config. `mode`, `ticks`, `tick_delay_ms`, `news_every_n_ticks`, `[[agents]]`. WARNING: sample `units = 1000` ≈ $1100 notional at 1.10 vs $10–100 stakes — longs reject at 1x margin by design; size down per broker minimums.
- `README.md` — product story. Matches code since P0 docs pass (live refuses, paper margin/split semantics documented).
- `data/run-<timestamp>.jsonl` — runtime output. Created on each run. Never read back currently.

## 2. Runtime dataflow (exact order)

1. CLI: `orchestrator [config.toml] [--resume]`. Load `RunConfig` (arg or default).
2. `licensing::check()` — no-op today.
3. Live-gate: `mode="live"` exits 1 (no live broker compiled in). Test mode continues.
4. If `--resume`: `latest_run_file("data")` + `load_snapshots()` BEFORE the new log file is created (else "latest" = this run's empty file).
5. Open new `EventLog` at `data/run-<ts>.jsonl`.
6. For each `AgentSpec`: build `Strategy`; stake = random in [min,max) or exactly min when equal; broker = `PaperBroker::restore()` if snapshot exists for the id else `new(stake)`; agent = `Agent::restore()` or `new()`; apply SL/TP; `reconcile()`.
7. One tokio task per agent on `broadcast::channel<TickMsg>(1024)` + one logger task on `mpsc`. Per tick each live agent emits events + a `Snapshot` every `snapshot_every_n_ticks` (50).
6. `orchestrator/src/main.rs:188-203` producer loop: `MockFeed::new(1.1000, 0.0006).next_price(config.agents[0].symbol)` + `MockNewsFeed::next_headline()`, broadcast same `TickMsg{candle, news}` to ALL agents.
7. Per agent per tick `agent-runtime/src/lib.rs:144-215` `on_tick(candle)`:
   - `broker.mark_price(candle.close)`, push history (cap 200)
   - emit `Tick{equity}`, death-check `equity<=0`
   - `check_risk_exits()` — if open position moved beyond SL/TP, force full close, skip strategy this tick
   - else `strategy.decide(ctx)` -> `broker.place_order(order)` -> `OrderPlaced` / `OrderRejected`
   - death-check again, split-check `equity > baseline*2.0` -> `broker.withdraw(equity/2)`, baseline = post-withdraw equity (defers with `OrderRejected` if cash insufficient)
8. `orchestrator/src/main.rs:236-265` `persist_event()` maps each `AgentEvent` to `{kind, data}` JSON and appends. `Tick` events are persisted too (noisy).

## 3. Key function signatures to open first

- Agent tick: `agent-runtime/src/lib.rs:144` `pub async fn on_tick(&mut self, candle: Candle) -> Vec<AgentEvent>`
- Risk: `agent-runtime/src/lib.rs:109` `async fn check_risk_exits(&mut self) -> Option<AgentEvent>`
- Fill: `broker-paper/src/lib.rs:44` `async fn place_order(&mut self, order: Order) -> Result<Fill, BrokerError>`
- Equity: `broker-paper/src/lib.rs` `fn account_state(&self) -> AccountState`
- Withdraw: `broker-paper/src/lib.rs:138` / trait default `core/src/lib.rs:127`
- Leverage: `broker-paper/src/lib.rs` `with_max_leverage()` (default 1.0)
- SMA: `strategy-sma/src/lib.rs:41` `async fn decide(&mut self, ctx: &MarketContext) -> Option<Order>`
- LLM: `strategy-llm/src/lib.rs:111` `decide()`, `strategy-llm/src/lib.rs:107` `is_healthy()`
- Hybrid: `strategy-hybrid/src/lib.rs:45` `decide()`
- Config: `trading-config/src/lib.rs:50` `RunConfig::load()`
- Log: `persistence/src/lib.rs:46` `append()`, `persistence/src/lib.rs:57` `read_all()` (currently unused by orchestrator)

## 4. Config shape (`config.toml`)

```toml
mode = "test" # "test" | "live" — live refuses to run since P0-3 (PaperBroker only)
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
- `G4` Tests (2026-09-13): 9 broker-paper + 7 agent-runtime + 3 persistence, `cargo test` green. Still no orchestrator/strategy tests.
- `G5` FIXED P0-2: split calls `broker.withdraw()`, baseline = post-withdraw equity. Defers (OrderRejected) while profit is unrealized — see `split_defers_while_profit_is_unrealized` test.
- `G6` FIXED P0-1: margin check is exposure-based both sides (closes always pass). 1x default via `with_max_leverage()`.
- `G7` FIXED P0-1: flip resets `avg_entry_price` to flip fill; partial close keeps old avg.
- `G8` FIXED P1-1: `--resume` rebuilds broker/baseline/withdrawn from latest `snapshot`/`final_summary` (`load_snapshots`; dead stay dead). Crash window = `snapshot_every_n_ticks` (50). Usage: `cargo run -p orchestrator -- config.toml --resume`.
