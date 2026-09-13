# PLAN — TraderY — For LLM agents

> Goal: make paper mode honest BEFORE adding any live broker. Do not add `broker-oanda`, real news, or analytics until P0 is green. Work in order P0 -> P1 -> P2. Each task lists files to touch, exact acceptance criteria, and how to verify.
>
> STATUS 2026-09-13: P0 DONE (14 tests green) + P1-1 DONE (19 tests: 9 broker-paper + 7 agent-runtime + 3 persistence). Crash resume via `--resume` works e2e. Next: P1-2.

## Global rules (obey on every task)

- `R1` Keep trait shape in `core/src/lib.rs`. Add methods with default bodies so existing impls still compile (see `Broker::reconcile()` pattern at `core/src/lib.rs:119-121`).
- `R2` Risk stays outside strategy. Edit `agent-runtime/src/lib.rs:109` and `broker-paper/src/lib.rs`, never bypass `check_risk_exits()`.
- `R3` Money stays `f64` for P0/P1. Note rounding TODOs, do not migrate to decimal yet.
- `R4` Run `cargo test` and `cargo build -p orchestrator` after every task. No `cargo run` with `mode="live"`.
- `R5` Update `docs/CODEMAP.md:G1-G8` and `README.md` caveats when behavior changes.

## P0 — Honest paper ✅ DONE 2026-09-13 (14 tests: 8 broker-paper + 6 agent-runtime)

### P0-1 Fix PaperBroker flip + margin ✅ DONE
- Landed: `broker-paper/src/lib.rs:44-148` exposure-based margin both sides (1x default, `with_max_leverage()`), closes always pass; flip resets entry, partial close keeps avg; `Broker::withdraw()` seam in `core/src/lib.rs:127` (loud default), cash-only impl in paper.
- Evidence: `buy_rejects_when_notional_exceeds_balance`, `sell_rejects_without_margin_cover`, `flip_resets_entry_to_new_fill`, `partial_close_keeps_average_entry`, `closing_order_always_allowed_so_sltp_can_exit`, `withdraw_reduces_balance_and_rejects_overdraft` (+2 more).
- Verify: `cargo test -p broker-paper` green.

### P0-2 Fix split to move money + stop loop ✅ DONE
- Landed: `agent-runtime/src/lib.rs:194-215` calls `broker.withdraw(equity/2)`, baseline = post-withdraw equity, strict `>` guard; deferral via `OrderRejected` when cash insufficient.
- Evidence: `split_fires_once_and_resets_baseline`, `split_defers_while_profit_is_unrealized`.
- Verify: `cargo test -p agent-runtime` green.

### P0-3 Gate live mode honestly ✅ DONE
- Landed: `orchestrator/src/main.rs:58-62` exits 1 on `mode="live"`; `README.md` caveat rewritten.
- Evidence: live config exits 1 with gate message; 5-tick test config exits 0.
- Verify: manual run (see above).

### P0-4 Tests for fill/SL-TP/death/split ✅ DONE (exit gate met)
- Landed: inline `#[cfg(test)]` in `broker-paper/src/lib.rs:151` (8) + `agent-runtime/src/lib.rs:216` (6).
- Verify: `cargo test` all green.

## P1 — Crash safety + inventory-aware strategies (do now, in order)

Agreed algo direction (educational, not advice): one strategy per agent per symbol from a registry — RSI / Donchian-breakout / ATR sizer + news-gate wrapper; LLM as regime router, algos as executors, risk layer unchanged. No spot-FX whale tracking (no consolidated tape).

### P1-1 Replay/reconcile ✅ DONE
- Landed: `snapshot` records every `snapshot_every_n_ticks` (default 50, 0 disables; old configs parse via serde default) + full-state `final_summary`; `persistence::{latest_run_file, load_snapshots, Snapshot}` (dead agents never resurrect); `PaperBroker::restore()` (validates position/entry match); `Agent::restore()` + `baseline()`; orchestrator `--resume` rebuilds from newest `data/run-*.jsonl` (loaded BEFORE the new log file is created); fixed-stake (`min==max`) `gen_range` panic found during testing.
- Known limits: crash window = up to N ticks; strategy internals (SMA memory) rebuild over ticks; corrupt snapshots fall back to fresh with a warning.
- Evidence: 3 persistence tests (round-trip+corrupt-skip, fold/latest-wins/no-resurrect, newest-file); `restore_*` tests in broker/agent; e2e fresh 6-tick run → `--resume` run prints `Resuming 'x' — balance=...` with prior balances.
- Verify: `cargo test -p persistence && cargo test -p agent-runtime` green.

### P1-2 Inventory-aware SMA
- Touch: `strategy-sma/src/lib.rs:41-58`
- Do: if `ctx.account.open_units != 0`, either return `None` (skip-pyramid) or emit flatten (opposite side, `units = abs(open_units)`) before reversing. Add `allow_pyramid: bool` + `max_units: f64` fields, wire from `trading-config/src/lib.rs:21-33` + `config.toml`.
- Accept: test with open long + bullish crossover -> no second buy when `allow_pyramid=false`.

### P1-3 Lag + feed fixes
- Touch: `orchestrator/src/main.rs:113-145,187-203`
- Do: count `Lagged(n)` per agent, emit `OrderRejected("tick-lag-skipped")` or log metric; change producer to per-symbol feeds (`HashMap<symbol, MockFeed>`) instead of `agents[0].symbol`.
- Accept: multi-symbol config fans out correctly; lag no longer silent.

## P2 — Sellable hardening (do last, demo-first, Rust-only)

Promotion ladder (no skipping steps): internal paper (`PaperBroker`) →
venue demo/practice account (real matching engine, fake money) → venue
live (real money). Each venue adapter serves demo AND live from one
crate via config (`base_url` + `api_key` + `account_id` + `allow_live`).
`mode="live"` stays refused until at least one venue adapter + P1
replay + stats thresholds exist. Hot path stays sync in-process Rust;
all network I/O (broker REST/WS, news polling, LLM calls) on spawned
tasks with timeouts + cooldowns — the zero-latency principle.

- `P2-1` Decimal money (`rust_decimal`), per-venue `min_units` /
  `min_notional` / spread / commission in config. Paper simulates them
  so a $100 stake is validated per venue BEFORE any demo run (many
  brokers min 1000 units ≈ $1100 notional — incompatible; OANDA-style
  1-unit minimums fit).
- `P2-2` Performance analytics + trackers (`analytics` crate, offline,
  reads logs via `read_all()`): equity curve, Sharpe/Sortino, max
  drawdown, win rate, profit factor, exposure — broken down per
  agent / symbol / strategy. Plus a log-tailing live tracker for the
  console. Promotion demo→live requires green thresholds here.
- `P2-3` Real `licensing::check()` signature verification (Ed25519 offline).
- `P2-4` Venue adapters, one crate each implementing `Broker` + real
  `reconcile()` (query open positions/orders on startup): start with
  `broker-oanda` (practice + live base URLs), then next venue by demand.
  Each lands with its demo config + recorded demo-run stats first.
- `P2-5` World-info feeds, one crate each implementing `NewsFeed`:
  economic calendar first (rates/CPI/NFP drive FX more than headlines),
  then headline APIs (e.g. Finnhub/AlphaVantage-style) with LLM
  sentiment scoring behind the existing `strategy-llm` pattern. Feed
  failures degrade to neutral (like the LLM cooldown) and never block
  trading. Powers the P1-2 news-gate (pause/reduce into high-impact
  events) and the LLM router.

## Explicit non-goals (do NOT do in P0/P1)

- No live trading, no real API keys, no HFT/market-making, no NautilusTrader port, no multi-process distribution.

## How to pick up work (LLM checklist)

1. Open `docs/CODEMAP.md`, then only the files listed in your P-task.
2. Open `docs/ANALYSIS.md:A1-A5` for bug context.
3. Implement smallest diff that meets Accept criteria.
4. Run `cargo test` + `cargo build -p orchestrator`. Paste failures, fix, repeat.
5. Update `docs/CODEMAP.md` gotchas if behavior changed.
