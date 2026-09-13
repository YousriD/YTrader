# ANALYSIS — TraderY — Verified 2026-09-13, updated post-P0 same day

> For LLM agents: every claim below was checked against current source. File:line refs are load-bearing — open them before editing. Each A-item carries FIXED (with fix ref) or OPEN status. P0 DONE: 14 tests green.

## Verdict (1 paragraph)

Well-shaped MVP skeleton, not a trading system yet. Architecture (trait split, risk-outside-strategy, hybrid fallback) is the strongest part. Economics and "live" path are mostly story. Keep the trait/orchestrator shape; make paper mode honest before adding any live broker.

## What works (keep)

- `W1` Crate split correct: `core/src/lib.rs:26,42,102,138` owns `MarketFeed/Broker/NewsFeed/Strategy`. New OANDA adapter = new crate, no rewrite. CONFIRMED.
- `W2` Risk outside strategy: `agent-runtime/src/lib.rs:165` `check_risk_exits()` runs before `strategy.decide()` at `:181`, and skips strategy after forced exit at `:172`. LLM hang cannot block SL/TP. CONFIRMED.
- `W3` LLM failure first-class: `strategy-llm/src/lib.rs:7-14` threshold=2, cooldown=30 ticks; `strategy-llm/src/lib.rs:107-109` `is_healthy()`; `strategy-hybrid/src/lib.rs:45-62` fallback + warm-keeping at `:61`. Shape correct. CONFIRMED. Warm-keeping detail is better than average.
- `W4` Config-driven agents: `trading-config/src/lib.rs:35-46` + `orchestrator/src/main.rs:72-105`. No recompile to retune. CONFIRMED.
- `W5` README honest about NautilusTrader rejection, small-account headwinds, bookkeeping-not-alpha (`README.md:72-74,109-117`). Better than typical FX repos. CONFIRMED.

## Where code diverges from story (fix before selling)

### A1. "Live" was a flag, not a mode — FIXED (P0-3)
- Was: `orchestrator/src/main.rs` always built `PaperBroker`; live only skipped `llm` agents.
- Now: `orchestrator/src/main.rs:58-62` refuses to run in live mode (`exit 1`, "no live broker compiled in"). `README.md` caveat rewritten to match. No `broker-live` crate yet — P2-4.
- Remaining: real live adapter + real `reconcile()` (P2-4). Do not re-enable live without it.

### A2. Event log was audit-only — FIXED (P1-1, snapshot-based)
- Was: `read_all()` had zero runtime callers; crash restarted everything fresh.
- Now: periodic `snapshot` records + full-state `final_summary`; `persistence::load_snapshots()` folds latest state per agent (dead stay dead); `orchestrator --resume` rebuilds broker/baseline/withdrawn. E2E-verified.
- Known limits: crash window up to `snapshot_every_n_ticks` (50); strategy memory rebuilds; live `reconcile()` still no-op until P2-4.

### A3. Split-at-double moved no money, and looped — FIXED (P0-2)
- Was: `on_tick` updated `baseline/total_withdrawn` only; `equity >= baseline*2` re-fired every tick.
- Now: `agent-runtime/src/lib.rs:194-215` calls `broker.withdraw(equity/2)`, baseline = post-withdraw equity, condition is strict `>`. Proven by `split_fires_once_and_resets_baseline` test.
- Known limitation (documented, tested): withdrawals are cash-only, so a split defers (`OrderRejected`) while profit is unrealized — see `split_defers_while_profit_is_unrealized`. Close winners first, then split.

### A4. Paper P&L was not FX — FIXED (P0-1)
- Was: buys rejected on `units*price > balance`, sells unchecked (infinite shorts); flips kept stale `avg_entry_price`.
- Now: `broker-paper/src/lib.rs:44-115` exposure-based margin both sides at 1x (`with_max_leverage()` to raise; closes always pass so SL/TP can exit); flips reset entry to the flip fill, partial closes keep old avg, flat resets to 0. Proven by 8 broker-paper tests.
- Still true: sample `config.toml` `units=1000` ≈ $1100 notional vs $10–100 stakes — longs correctly reject at 1x. Size down per broker minimums (README + PLAN P1-2 sizing work remain).
- Nuance (unchanged): spread IS modeled (1.2 pips). Still missing: commission, per-symbol config.

### A5. Strategies do not know inventory — OPEN (P1-2 next)
- Evidence: `strategy-sma/src/lib.rs:41-52` uses only `ctx.history`, ignores `ctx.account`. Fixed `units`. Can add to winner, reverse without flattening, fight open SL/TP position.
- LLM does see `balance/equity/open_units` in prompt (`strategy-llm/src/lib.rs:135-149`) but still emits fixed `units`, no flatten signal.
- Agreed direction (not investment advice): one strategy per agent per symbol from a registry (RSI / Donchian / ATR sizer + news-gate; LLM as regime router, algos as executors). No spot-FX "whale tracking" — no consolidated tape; COT/sentiment proxies at most. See `docs/PLAN.md:P1-2`.

### A6. Other gaps — status per item
- `T1` Tests — FIXED for P0 scope: 8 broker-paper + 6 agent-runtime, `cargo test` green. Still no orchestrator/persistence/strategy tests (P1+).
- `T2` Tick lag dropped silently — OPEN: `orchestrator/src/main.rs:144` `Lagged(_) => continue`, no counter/log. See P1-3.
- `T3` One mock walk shared — OPEN: single `MockFeed`, `config.agents[0].symbol` only. See P1-3.
- `T4` Money is `f64` everywhere — OPEN by decision until P2-1.
- `T5` Licensing is env-var presence — OPEN until P2-3.

## What the original analysis missed
- `M1` Split infinite-loop (A3 extension above) — more urgent than "reporting vs capital".
- `M2` Per-agent `symbol` ignored for feed (T3 extension) — multi-symbol TODO is actually single-symbol bug today.
- `M3` README live-warning inversion (A1) — docs actively mislead about risk direction.

## Bottom line for next LLM
P0 + P1-1 DONE (19 tests). Work P1 in order: P1-2 inventory-aware strategy registry, P1-3 per-symbol feeds + lag counting. No `broker-oanda` until P1 lands.
