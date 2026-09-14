# ANALYSIS — TraderY — Verified 2026-09-13, updated post-P2-4 same day

> For LLM agents: every claim below was checked against current source. Each A-item carries FIXED (with fix ref) or OPEN status. P0 + P1 + P2-1-venue + P2-2 + P2-4 DONE: 69 tests green.

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

### A5. Strategies were inventory-blind — FIXED (P1-2)
- Was: SMA used only `ctx.history`, fixed size, pyramided into SL/TP positions.
- Now: `strategy-indicators` registry (`Rsi`, `DonchianBreakout`, `AtrSizer`, `NewsGate`) + retrofitted SMA all share `inventory::{pyramid_blocked, clamp_units}` — no adds to open positions unless `allow_pyramid`, sizes capped at `max_position_units`. Per-agent knobs in `config.toml`, recursive `atr`/`news_gated` nesting, `contains_llm()` gating at any depth.
- Known limits: skip-only (no auto-flatten); ATR sizer margin-unaware (broker rejects, correctly); LLM-as-router not built.
- Fix ref: `docs/PLAN.md:P1-2`.

### A6. Other gaps — status per item
- `T1` Tests — DONE for P0+P1 scope: 9 broker-paper + 7 agent-runtime + 3 persistence + 14 strategy-indicators + 3 trading-config + 3 orchestrator + 1 resume-e2e = 40 green. Still no strategy-llm/hybrid/feed/news tests.
- `T2` Tick lag dropped silently — FIXED (P1-3): counted per agent, printed + persisted as `tick_lag`, totals in final report.
- `T3` One mock walk shared — FIXED (P1-3): one feed per distinct symbol; news still global (mock has no attribution).
- `T4` Money is `f64` — OPEN by decision (P2-1 deferred decimal: rewrite risk dwarfs rounding risk; tolerance discipline in tests). Venue-economics half of P2-1 is DONE (minimums/commission/spread simulated per agent).
- `T5` Licensing is env-var presence — OPEN until P2-3.

## What the original analysis missed
- `M1` Split infinite-loop (A3 extension above) — more urgent than "reporting vs capital".
- `M2` Per-agent `symbol` ignored for feed (T3 extension) — multi-symbol TODO is actually single-symbol bug today.
- `M3` README live-warning inversion (A1) — docs actively mislead about risk direction.

## Bottom line for next LLM
P0 + P1 + P2-1-venue + P2-2 + P2-4 DONE (69 tests). Next: P2-5 feeds (P2-3 licensing when selling nears). Live mode exists but ONLY against the OANDA practice host with fail-closed reconcile — the trade host is refused mechanically, and `gate` still blocks promotable claims.
