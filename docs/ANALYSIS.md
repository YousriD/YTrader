# ANALYSIS — TraderY — Verified 2026-09-13

> For LLM agents: every claim below was checked against current source. File:line refs are load-bearing — open them before editing. Status = CONFIRMED unless marked NUANCE.

## Verdict (1 paragraph)

Well-shaped MVP skeleton, not a trading system yet. Architecture (trait split, risk-outside-strategy, hybrid fallback) is the strongest part. Economics and "live" path are mostly story. Keep the trait/orchestrator shape; make paper mode honest before adding any live broker.

## What works (keep)

- `W1` Crate split correct: `core/src/lib.rs:26,42,102,138` owns `MarketFeed/Broker/NewsFeed/Strategy`. New OANDA adapter = new crate, no rewrite. CONFIRMED.
- `W2` Risk outside strategy: `agent-runtime/src/lib.rs:165` `check_risk_exits()` runs before `strategy.decide()` at `:181`, and skips strategy after forced exit at `:172`. LLM hang cannot block SL/TP. CONFIRMED.
- `W3` LLM failure first-class: `strategy-llm/src/lib.rs:7-14` threshold=2, cooldown=30 ticks; `strategy-llm/src/lib.rs:107-109` `is_healthy()`; `strategy-hybrid/src/lib.rs:45-62` fallback + warm-keeping at `:61`. Shape correct. CONFIRMED. Warm-keeping detail is better than average.
- `W4` Config-driven agents: `trading-config/src/lib.rs:35-46` + `orchestrator/src/main.rs:72-105`. No recompile to retune. CONFIRMED.
- `W5` README honest about NautilusTrader rejection, small-account headwinds, bookkeeping-not-alpha (`README.md:72-74,109-117`). Better than typical FX repos. CONFIRMED.

## Where code diverges from story (fix before selling)

### A1. "Live" is a flag, not a mode — SEVERITY: HIGH — CONFIRMED
- Evidence: `orchestrator/src/main.rs:73-76` live only skips `llm` agents. `orchestrator/src/main.rs:96` always builds `Box::new(PaperBroker::new(stake))`. No `broker-live`, no branch.
- Misleading counterpart: `README.md:118-119` warns flipping to live = financial risk. Today live = paper, so false danger now, zero safety later.
- Fix: either gate `Mode::Live` to refuse-to-run without a real broker, or rename to `paper-only`. See `docs/PLAN.md:P0`.

### A2. Event log is audit trail, not event sourcing — SEVERITY: HIGH — CONFIRMED
- Evidence: `persistence/src/lib.rs:46` `append()` used at `orchestrator/src/main.rs:168,179,263`; `persistence/src/lib.rs:57` `read_all()` never called by orchestrator (grep: zero callers). `agent-runtime/src/lib.rs:100-104` `reconcile()` logs error only, paper no-op.
- Effect: crash restarts with fresh `PaperBroker::new(stake)`, fresh baseline, phantom vs real positions diverge.
- Fix: replay or reconcile on startup. See `docs/PLAN.md:P1`.

### A3. Split-at-double does not move money, and loops — SEVERITY: HIGH — CONFIRMED + EXTENDED
- Evidence: `agent-runtime/src/lib.rs:194-200` updates `baseline/total_withdrawn`, emits `Split{withdrawn, new_baseline}`, never touches `broker.balance`.
- Extension beyond original analysis: `new_baseline = equity/2`, so condition `equity >= baseline*2` stays true. Once doubled, agent emits `Split` EVERY tick until equity dips. Not just reporting — infinite-report loop.
- Fix: debit broker equity (needs `Broker::withdraw()` seam) or spawn child agent; set `baseline = equity_after_withdraw`. See `docs/PLAN.md:P0`.

### A4. Paper P&L is not FX — SEVERITY: HIGH — CONFIRMED
- Evidence buy/sell asymmetry: `broker-paper/src/lib.rs:46-48` rejects `Buy` if `units*price > balance`, no check for `Sell`. $40 agent can short 1000 units freely, stack infinitely.
- Evidence flip bug: `broker-paper/src/lib.rs:63-77` realizes P&L on `closing_units` but leaves `avg_entry_price` unchanged when flipping sides (e.g. long 1000 -> sell 2000 leaves short -1000 priced at old long entry). Zero-reset at `:75-77` only fires when flat.
- Evidence sizing: `config.toml:11,21,33` `units=1000`, EURUSD ~1.10 -> $1100 notional vs $10-100 stake. Most longs -> `InsufficientBalance`.
- Nuance: spread IS modeled (`broker-paper/src/lib.rs:25-32,40-43`, 1.2 pips). Missing is margin/leverage/commission, not spread.
- Fix: margin + inventory-aware sizing. See `docs/PLAN.md:P0`.

### A5. Strategies do not know inventory — SEVERITY: MEDIUM — CONFIRMED
- Evidence: `strategy-sma/src/lib.rs:41-52` uses only `ctx.history`, ignores `ctx.account`. Fixed `units`. Can add to winner, reverse without flattening, fight open SL/TP position.
- LLM does see `balance/equity/open_units` in prompt (`strategy-llm/src/lib.rs:135-149`) but still emits fixed `units`, no flatten signal.
- Fix: pass inventory to sizing, add flatten/skip-if-open. See `docs/PLAN.md:P1`.

### A6. Other gaps — ALL CONFIRMED
- `T1` No tests: zero `#[test]`, zero `tests/` dirs.
- `T2` Tick lag dropped silently: `orchestrator/src/main.rs:144` `Lagged(_) => continue`, no counter/log. Hits slow LLM agents first (8s timeout at `strategy-llm/src/lib.rs:53`).
- `T3` One mock walk shared: `orchestrator/src/main.rs:188` single `MockFeed`, `orchestrator/src/main.rs:196,201` uses `config.agents[0].symbol` only, broadcasts same candle to all.
- `T4` Money is `f64` everywhere (`core/src/lib.rs:15-21,70-77`, broker balances). No fixed-point.
- `T5` Licensing is env-var presence: `licensing/src/lib.rs:15-25` `TRADING_LICENSE_KEY` exists -> `Valid{owner:"unverified"}`.

## What the original analysis missed
- `M1` Split infinite-loop (A3 extension above) — more urgent than "reporting vs capital".
- `M2` Per-agent `symbol` ignored for feed (T3 extension) — multi-symbol TODO is actually single-symbol bug today.
- `M3` README live-warning inversion (A1) — docs actively mislead about risk direction.

## Bottom line for next LLM
Do `docs/PLAN.md:P0` first (honest paper broker + honest split + honest live-gate). Do not add `broker-oanda`, multi-symbol feeds, or analytics before P0 tests pass.
