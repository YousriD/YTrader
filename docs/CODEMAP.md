# CODEMAP — TraderY — READ THIS FIRST

> For LLM agents: this is the file-to-behavior index. Start here, then open only the files listed for your task. Do not guess paths.

## 1. Workspace members (all in `C:\Repos\TraderY\`)

| Crate | Path | Role | Key exports |
|---|---|---|---|
| `core` | `core/src/lib.rs` | Trait contracts. No logic. | `Broker` (+`withdraw` at :127, `reconcile` at :119), `MarketFeed`, `NewsFeed`, `Strategy`, `Order`, `Side`, `Fill`, `AccountState`, `Candle`, `NewsItem`, `MarketContext`, `RunMode`, `BrokerError` |
| `broker-paper` | `broker-paper/src/lib.rs` | Simulated fills. | `PaperBroker::new()` / `restore()`, `with_max_leverage()` (1x), venue: `with_min_units/notional/commission/spread_pips`, `place_order()`, `withdraw()` (12 tests) |
| `broker-oanda` | `broker-oanda/src/lib.rs` | P2-4 REAL adapter, practice-host only. | `OandaBroker::new(url, key, account, symbol)`, `is_practice_url()`, `parse_fill/parse_balance/parse_position/parse_mid/map_api_error`, genuine `reconcile()`, `withdraw()` always errors (8 tests + 1 ignored live) |
| `feed-mock` | `feed-mock/src/lib.rs` | Random-walk prices. | `MockFeed::new(start_price, vol)`, `next_price()` |
| `news-mock` | `news-mock/src/lib.rs` | Fake headlines. | `MockNewsFeed::new(every_n_ticks)`, `next_headline()` (global fan-out; no attribution) |
| `news-calendar` | `news-calendar/src/lib.rs` | P2-5 REAL calendar (keyless weekly JSON). | `CalendarFeed` (hourly cache, dedup, degrades silent), `parse_calendar()`, `Impact`, `CalendarEvent`, `fetch_events()` (5 tests + 1 ignored live) |
| `strategy-sma` | `strategy-sma/src/lib.rs` | SMA crossover baseline, inventory-guarded. | `SmaCrossover::new(name, fast, slow, units)`, `with_inventory()`, `decide()` |
| `strategy-indicators` | `strategy-indicators/src/` | P1-2 registry + P2-5 calendar gate. | `Rsi`, `DonchianBreakout`, `AtrSizer`, `NewsGate`, `CalendarGate` (`currencies_of`, `suppressed`), `inventory` guards (18 tests) |
| `strategy-router` | `strategy-router/src/` | P2-6 Tier 2 router, dual-brain. | `RouterBrain`, `RuleRouter` (drift/vol classifier), `LlmRouter` (cooldown), `RouterStrategy` (validate→rule→default chain, warm-keeping, `active_strategy`) (9 tests) |
| `strategy-llm` | `strategy-llm/src/lib.rs` | LLM every N ticks. | `LlmStrategy::new(name, api_key, units, persona)`, `ask_claude()`, `decide()`, `is_healthy()` |
| `strategy-hybrid` | `strategy-hybrid/src/lib.rs` | LLM + SMA fallback. | `HybridStrategy::new(name, primary, fallback)`, `decide()` |
| `agent-runtime` | `agent-runtime/src/lib.rs` | Lifecycle + risk layer. | `Agent::new()`, `Agent::restore()`, `baseline()`, `with_stop_loss()`, `with_take_profit()`, `reconcile()`, `check_risk_exits()` (:109), `on_tick()` (:144), split-withdraw at :194-215, `AgentEvent`, `AgentStatus`. Tests (7). |
| `persistence` | `persistence/src/lib.rs` | JSONL append-only log + resume fold. | `EventLog::open()`, `append()`, `EventLog::read_all()`, `latest_run_file()`, `load_snapshots()`, `Snapshot`. Tests (3). |
| `trading-config` | `trading-config/src/lib.rs` | `config.toml` parser. | `RunConfig::load()`, `Mode`, `AgentSpec`, `StrategySpec`, `snapshot_every_n_ticks` (default 50) |
| `licensing` | `licensing/src/lib.rs` | No-op seam. | `check()` -> `LicenseStatus::Unlicensed/Valid/Invalid` |
| `orchestrator` | `orchestrator/src/main.rs` | Binary, wiring only. | `main()`, `--resume` flag, `TickMsg`, `LogMsg` (Event/Snapshot/TickLag/Final), `build_strategy()`, `print_event()` (fills show price) |
| `analytics` | `analytics/src/` | P2-2 offline stats: reconstruction, metrics, gate, CLI. | `reconstruct()`, `analyze()`, `evaluate()`, `Thresholds`, `report/gate/track` cmds (17 tests) |

Root files:
- `Cargo.toml` — workspace members + shared deps (tokio full, async-trait, rand 0.8, serde, serde_json, reqwest rustls-tls, chrono clock+serde)
- `config.toml` — single run config. `mode`, `ticks`, `tick_delay_ms`, `news_every_n_ticks`, `[[agents]]`. WARNING: sample `units = 1000` ≈ $1100 notional at 1.10 vs $10–100 stakes — longs reject at 1x margin by design; size down per broker minimums.
- `README.md` — product story. Matches code since P0 docs pass (live refuses, paper margin/split semantics documented).
- `data/run-<timestamp>.jsonl` — runtime output. Created on each run. Never read back currently.

## 2. Runtime dataflow (exact order)

1. CLI: `orchestrator [config.toml] [--resume]`. Load `RunConfig` (arg or default).
2. `licensing::check()` — no-op today.
3. Live-gate (P2-4): `mode="live"` requires practice host + `[oanda] account_id` + `OANDA_API_KEY` + distinct symbols + per-agent opening reconcile — ANY failure exits 1 (fail closed). Test mode continues on paper.
4. If `--resume`: `latest_run_file("data")` + `load_snapshots()` BEFORE the new log file is created (else "latest" = this run's empty file).
5. Open new `EventLog` at `data/run-<ts>.jsonl`.
6. For each `AgentSpec`: recursive `build_strategy()` (registry kinds + nested `atr`/`news_gated`; `contains_llm()` gates keyless/LLM); stake = random in [min,max) or exactly min when equal; broker = `PaperBroker::restore()` if snapshot exists for the id else `new(stake)`; agent = `Agent::restore()` or `new()`; apply SL/TP; `reconcile()`.
7. One tokio task per agent, subscribed to its symbol's `broadcast::channel<TickMsg>(1024)` (one `MockFeed` per distinct symbol via `symbols_of()`), + one logger task on `mpsc`. Per tick each live agent emits events + a `Snapshot` every `snapshot_every_n_ticks` (50); `Lagged(n)` is counted, printed + persisted as `tick_lag`.
8. Producer loop: per symbol per tick, `feed.next_price(symbol)` + shared `MockNewsFeed` headline fanned out to all symbols.
9. Per agent per tick `agent-runtime/src/lib.rs:144-215` `on_tick(candle)`:
   - `broker.mark_price(candle.close)`, push history (cap 200)
   - emit `Tick{equity}`, death-check `equity<=0`
   - `check_risk_exits()` — if open position moved beyond SL/TP, force full close, skip strategy this tick
   - else `strategy.decide(ctx)` -> `broker.place_order(order)` -> `OrderPlaced` / `OrderRejected`
   - death-check again, split-check `equity > baseline*2.0` -> `broker.withdraw(equity/2)`, baseline = post-withdraw equity (defers with `OrderRejected` if cash insufficient)
10. Logger maps each `AgentEvent` to `{kind, data}` JSON and appends (`Tick` included — noisy). Final report line per agent adds `lagged=incidents/ticks`.

## 3. Key function signatures to open first

- Agent tick: `agent-runtime/src/lib.rs:144` `pub async fn on_tick(&mut self, candle: Candle) -> Vec<AgentEvent>`
- Risk: `agent-runtime/src/lib.rs:109` `async fn check_risk_exits(&mut self) -> Option<AgentEvent>`
- Fill: `broker-paper/src/lib.rs:44` `async fn place_order(&mut self, order: Order) -> Result<Fill, BrokerError>`
- Equity: `broker-paper/src/lib.rs` `fn account_state(&self) -> AccountState`
- Withdraw: `broker-paper/src/lib.rs:138` / trait default `core/src/lib.rs:127`
- Leverage: `broker-paper/src/lib.rs` `with_max_leverage()` (default 1.0)
- SMA: `strategy-sma/src/lib.rs` `decide()` + `with_inventory()` (P1-2 guard)
- Registry: `strategy-indicators` — `Rsi::rsi()`, `DonchianBreakout::breakout()`, `AtrSizer::atr()`, `NewsGate`, `inventory::{pyramid_blocked, clamp_units}` (14 tests)
- Builder: `orchestrator/src/main.rs` `build_strategy()` (recursive), `contains_llm()`, `symbols_of()`
- LLM: `strategy-llm/src/lib.rs:111` `decide()`, `strategy-llm/src/lib.rs:107` `is_healthy()`
- Hybrid: `strategy-hybrid/src/lib.rs:45` `decide()`
- Config: `trading-config/src/lib.rs:50` `RunConfig::load()`
- Log: `persistence/src/lib.rs:46` `append()`, `persistence/src/lib.rs:57` `read_all()` (currently unused by orchestrator)

## 4. Config shape (`config.toml`)

```toml
mode = "test" # "test" | "live" — live = OANDA practice ONLY (trade host refused mechanically)
ticks = 500
tick_delay_ms = 50
news_every_n_ticks = 15
[[agents]]
id = "algo-fast"
symbol = "EUR_USD"  # one feed per distinct symbol (P1-3); same-symbol agents share a walk
stake_min = 10.0
stake_max = 100.0
units = 1000.0      # WARNING: ≈$1100 notional at 1.10 — correctly rejected at 1x on $10-100 stakes; size down
allow_pyramid = false        # default: skip entries while a position is open
max_position_units = 1000.0  # default: units
stop_loss_pct = 0.005
take_profit_pct = 0.01
strategy = { kind = "sma", fast = 5, slow = 20 }
# strategy = { kind = "rsi", period = 14, overbought = 70.0, oversold = 30.0 }
# strategy = { kind = "donchian", channel = 20 }
# strategy = { kind = "atr", inner = { kind = "donchian", channel = 20 }, period = 14, risk_pct = 0.02, max_units = 300.0 }
# strategy = { kind = "llm", persona = "...", fallback_fast = 5, fallback_slow = 20 }
```

## 5. Gotchas for LLMs (do not re-discover)

- `G1` FIXED P1-3: one feed per distinct symbol (`symbols_of()`); same-symbol agents share a walk, symbols never cross. News still fanned out globally (mock has no attribution).
- `G2` FIXED P1-3: `Lagged(n)` counted per agent, printed + persisted as `tick_lag`, totals in final report (`lagged=i/t`). Still hits slow LLM agents first (8s timeout).
- `G3` Money is `f64` everywhere. No decimal type. Tests compare with tolerance, never `==`.
- `G4` Tests: 95 green — 12 broker-paper + 8 broker-oanda (+1 ignored live) + 9 agent-runtime + 3 persistence + 18 strategy-indicators + 7 trading-config + 6 orchestrator + 1 resume-e2e + 17 analytics + 5 news-calendar (+1 ignored live) + 9 strategy-router. Still no strategy-llm/hybrid/feed-mock/news-mock tests.
- `G9` Log fill shapes (P2-2) + regime events (P2-6): `order_placed{price}`, `stop_loss/take_profit_hit{closed_units}`, `regime_selected{strategy}`. Reconstruction mirrors broker average-cost matching; snapshots re-seed it; orphan closes counted, never invented.
- `G5` FIXED P0-2: split calls `broker.withdraw()`, baseline = post-withdraw equity. Defers (OrderRejected) while profit is unrealized — see `split_defers_while_profit_is_unrealized` test.
- `G6` FIXED P0-1: margin check is exposure-based both sides (closes always pass). 1x default via `with_max_leverage()`.
- `G7` FIXED P0-1: flip resets `avg_entry_price` to flip fill; partial close keeps old avg.
- `G8` FIXED P1-1: `--resume` rebuilds broker/baseline/withdrawn from latest `snapshot`/`final_summary` (`load_snapshots`; dead stay dead). Crash window = `snapshot_every_n_ticks` (50). Usage: `cargo run -p orchestrator -- config.toml --resume`.
