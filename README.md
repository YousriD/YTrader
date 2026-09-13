# Autonomous FX Trading System — MVP

A Rust workspace built to scale: every external dependency (broker, price
feed, news source, strategy "brain") sits behind a trait in `core`. This
MVP wires up mock/paper implementations of all of them end-to-end so you
can run the full agent lifecycle — spawn, trade, die at zero, split at
double — today, with zero external accounts or API keys required.

## Quick start

```bash
cd trading-system
cargo run -p orchestrator            # reads ./config.toml by default
cargo run -p orchestrator my-config.toml   # or point at a specific file
```

Agents are now defined entirely in `config.toml` — no code changes or
recompiling needed to add, remove, or retune an agent. Edit the file,
run again.

To also run the LLM-driven agent defined in the sample config:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p orchestrator
```

Every event (order, stop-loss, take-profit, split, death) is printed
to the console **and** durably appended to `data/run-<timestamp>.jsonl`
as it happens — that file is the audit trail and the thing you'd
replay to reconstruct state after a crash.

> **Note on this environment:** built in a sandbox without network
> access, so `cargo build` hasn't been run here. Treat the first local
> build as a validation pass.

## Workspace layout

| Crate | Role |
|---|---|
| `core` | Trait definitions: `Broker`, `MarketFeed`, `NewsFeed`, `Strategy`. The whole extension contract. |
| `broker-paper` | Simulated fills against a mark-to-market price, with spread. |
| `feed-mock` | Random-walk price generator, no network needed. |
| `news-mock` | Emits sample headlines with sentiment scores periodically. |
| `strategy-sma` | Algorithmic baseline: fast/slow SMA crossover. |
| `strategy-llm` | Calls Claude per tick (well, every N ticks), with its own failure backoff/cooldown. |
| `strategy-hybrid` | Wraps an LLM strategy with an algorithmic fallback — the "AI unreachable" guarantee lives here. |
| `agent-runtime` | Agent lifecycle: stop-loss/take-profit risk layer, die-at-zero, split-at-double, reconcile-on-startup hook. |
| `persistence` | Append-only JSONL event log — durability and audit trail. |
| `trading-config` | Parses `config.toml` into typed agent specs. |
| `licensing` | Currently a no-op seam — the hook where real license enforcement goes before you sell this. |
| `orchestrator` | Binary: loads config, builds the agent pool, runs everything as parallel tokio tasks, logs + persists. |

## The rules you specified, and where they live

- **Start with $10–$100:** `stake_min`/`stake_max` per agent in `config.toml`.
- **Die at zero:** `agent-runtime::Agent::on_tick` — checked before and after every order.
- **Split 50/50 at double:** same function. Repeats every time it doubles again.
- **Stop-loss / take-profit:** `agent-runtime::Agent::check_risk_exits` — runs *before* the strategy is consulted each tick, independent of whatever the strategy decided. Set per-agent in config; omit to disable.
- **TEST vs LIVE:** `trading_config::Mode`. In `live`, the orchestrator structurally refuses to build any `llm`-strategy agent — this isn't a setting you can misconfigure, it's a hard branch in `main.rs`.
- **AI unreachable/rate-limited → system keeps operating:** `strategy-llm` tracks consecutive failures and enters a cooldown (no more API calls for N ticks) rather than hanging or erroring. `strategy-hybrid` wraps it with an algorithmic fallback that takes over automatically during that cooldown — the agent never stalls.
- **Parallel threads:** the orchestrator runs on a multi-threaded tokio runtime; each agent is an independent spawned task fed ticks over a broadcast channel. A slow LLM call in one agent never blocks the others.
- **Mix of agents/algorithms, "crazy approaches" allowed:** any `Strategy` implementation is a peer to any other, defined declaratively per-agent in config.

## Architecture decisions made now, deliberately, because they're expensive later

- **Event sourcing, not just in-memory state.** Every state change is an append-only record (`persistence::EventLog`). Retrofitting this after strategies/agents assume in-memory-only state is painful; doing it now costs almost nothing.
- **Config-driven agents.** Even as a single-user tool, a config file (vs. hardcoded `main.rs`) is what makes this distributable later without you personally recompiling it for a buyer.
- **`Broker::reconcile()` as a required seam.** A paper broker has nothing to reconcile, but the hook exists so a live adapter is *forced* to ask the real broker "what's actually open?" on every startup — this is the difference between a crash-and-restart being a non-event and it being a double-position disaster.
- **Licensing seam, not licensing.** `licensing::check()` does nothing meaningful today (env var presence only) — but `main()` already calls through it, so adding real enforcement later is a one-file change, not a restructuring.

## Why NOT NautilusTrader (for this specific project)

Seriously considered it — it's a mature, Rust-core, backtest/live-parity engine with a huge integration surface. But its concurrency model is a deliberate single-threaded, deterministic core per node (LMAX-disruptor style, for reproducible backtests); its own docs say running multiple engine instances concurrently *in the same process* isn't supported. That's close to the opposite of what this project needs — many small, independent, genuinely parallel agents racing each other with a kill/split economics model on top. Adopting it would mean inheriting its concurrency philosophy (and its Python control-plane surface) instead of the one you actually asked for. Revisit this if the product ever pivots toward market-making/HFT-style strategies where deterministic single-threaded execution is the point.

## What "zero latency" actually means here

True zero latency isn't physically possible. What's real: the hot path
(mark-to-market, algorithmic strategy decisions, stop-loss/take-profit
checks, order placement against the paper broker) is synchronous,
in-process Rust running as its own task per agent. LLM-driven agents
are isolated on the same footing as any other strategy — their latency
(and now, their failures) never blocks or slows down the other agents
running alongside them.

## Next steps toward a sellable single-user tool

1. **Real broker adapter** (`broker-oanda` or similar): implement
   `Broker` for real, including a real `reconcile()` that queries open
   positions/orders from the API on startup.
2. **Real news feed**: implement `NewsFeed` against a real news/calendar
   API, optionally routing headlines through Claude for sentiment
   scoring first (same pattern `strategy-llm` already uses).
3. **Multi-symbol support**: the mock feed and orchestrator currently
   assume one symbol across all agents (`config.agents[0].symbol`).
   Real usage will want one feed subscription per distinct symbol,
   fanned out to the agents watching it.
4. **Performance analytics**: compute Sharpe ratio, max drawdown, and
   win rate from the event log — this is what actually makes the
   product's track record legible to a future buyer, not just raw
   equity numbers.
5. **Real license enforcement**: replace `licensing::check()`'s env-var
   presence check with actual signature verification before
   distributing to anyone else.
6. **Position sizing / real risk limits**: validate in paper mode
   whether a $10–100 account can even clear your chosen broker's
   minimum lot size before writing more strategies.

## Honest caveats

- This is software, not investment advice, and I'm not a financial
  advisor — the risk-management logic (kill-at-zero, split-at-double)
  is bookkeeping I built to your spec, not a claim that the underlying
  strategies are profitable.
- Small accounts face real structural headwinds (minimum lot sizes,
  spread cost as a % of capital, broker minimums) that no amount of
  clever code removes. Worth stress-testing in paper mode first.
- `mode = "live"` is currently disabled by the orchestrator (it exits instead
  of trading) because only `PaperBroker` exists — there is no real execution
  yet. Do not treat live as paper-with-more-risk. See `docs/PLAN.md:P0-3`.
