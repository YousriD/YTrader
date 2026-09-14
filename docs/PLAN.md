# PLAN — TraderY — For LLM agents

> Goal: make paper mode honest BEFORE adding any live broker. Do not add `broker-oanda`, real news, or analytics until P0 is green. Work in order P0 -> P1 -> P2. Each task lists files to touch, exact acceptance criteria, and how to verify.
>
> STATUS 2026-09-13: P0 + P1 + P2-1-venue + P2-2 + P2-4 DONE (69 tests). OANDA practice adapter live with mechanical guards; real-money host refused. Next: P2-5 feeds (P2-3 licensing when selling nears).

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

### P1-2 Strategy registry + inventory guards ✅ DONE
- Landed: new `strategy-indicators` crate — `Rsi` (Wilder exits), `DonchianBreakout` (strict channel breaks), `AtrSizer` (volatility decorator: `equity*risk_pct/atr` clamped), `NewsGate` (suppress N ticks after strong-sentiment news), shared `inventory::{pyramid_blocked, clamp_units}`. SMA retrofitted with `with_inventory()` + same guards. `StrategySpec` gains `rsi/donchian/atr/news_gated` (recursive nesting); `AgentSpec` gains `allow_pyramid` (default false) + `max_position_units` (default = units); orchestrator builds via recursive `build_strategy()` + `contains_llm()` gating at any depth; sample `config.toml` carries commented registry examples.
- Known limits: guard is skip-only (no auto-flatten — SL/TP exits); ATR sizer is margin-unaware (broker rejects unaffordable sizes, correctly but noisily — consider balance-aware cap as follow-up); LLM-as-regime-router not yet built (LLM still direct-places via Hybrid).
- Evidence: 14 indicator tests (synthetic sequences: RSI exit-buy, flat-silence, pyramid-block; Donchian both directions + flat; ATR calm-vs-wild sizing + pre-ready passthrough; gate trip/cooldown/recover + weak-news passthrough) + 3 config parse tests (nested wrappers, old-config compat, opt-in) + 2 orchestrator builder tests + live 30-tick registry run (nested ATR(NewsGate(Donchian)) placed orders through all 3 layers).
- Verify: `cargo test -p strategy-indicators -p trading-config` green.

### P1-3 Per-symbol feeds + lag accounting ✅ DONE
- Landed: `symbols_of()` fan-out key; one `MockFeed` + broadcast channel per distinct symbol (same-symbol agents share a walk; symbols never cross); global news fan-out kept (mock news has no symbol attribution — documented); `Lagged(n)` counted per agent, printed + persisted as `tick_lag` records, totals in final report (`lagged=incidents/ticks`).
- Manual procedure (per prompt: test or documented procedure): run the two-symbol config (`eu-sma` on EUR_USD + `gb-donch` on GBP_USD, 10 ticks) — observed per-symbol orders at different ticks (`gb-donch` Sell GBP_USD t4, `eu-sma` Buy EUR_USD t6) and `lagged=0/0` lines. Independence is structural: separate feed instances + separate RNG draws per symbol key; histories are per-agent and fed only from their symbol channel.
- Evidence: `symbols_of` unit test + the e2e run above.
- Verify: `cargo test -p orchestrator` green.

## P2 — Sellable hardening (do last, demo-first, Rust-only)

Promotion ladder (no skipping steps): internal paper (`PaperBroker`) →
venue demo/practice account (real matching engine, fake money) → venue
live (real money). Each venue adapter serves demo AND live from one
crate via config (`base_url` + `api_key` + `account_id` + `allow_live`).
`mode="live"` stays refused until at least one venue adapter + P1
replay + stats thresholds exist. Hot path stays sync in-process Rust;
all network I/O (broker REST/WS, news polling, LLM calls) on spawned
tasks with timeouts + cooldowns — the zero-latency principle.

- `P2-1` Venue economics ✅ DONE (decimal explicitly deferred, see below).
  Landed: `PaperBroker` venue rulebook — `min_units` (default 1.0),
  `min_notional` (default off), `commission_per_unit` on every fill
  incl. forced SL/TP exits, `spread_pips` override; per-agent config
  knobs (all serde-optional) applied on fresh AND resume paths via
  `apply_venue_economics()`; sample `config.toml` example. E2E: $100
  agent with $0.01/unit commission → $99.47 after one round.
  DECISION (documented, reversible): `rust_decimal` migration deferred.
  13-crate f64→Decimal rewrite is high-risk / near-zero behavioral gain
  at this stage — tests already enforce tolerance discipline (`never
  ==` rule, CODEMAP G3) and amounts are small. Revisit only if cumulative
  rounding shows in analytics, or when a venue adapter demands exact
  decimal serialization.
- `P2-2` Performance analytics + trackers ✅ DONE.
  Landed: `analytics` crate (lib + CLI) — broker-identical average-cost
  trade reconstruction (snapshot-seeded, orphan closes counted not
  invented, pre-price logs skipped); per-agent + portfolio metrics on
  total-value curves (splits never fake drawdowns): drawdown, Sharpe/
  Sortino per-tick, win rate, profit factor, expectancy, time-in-market;
  `Thresholds` + `evaluate()` demo→live gate (defaults: 20 trades, PF
  ≥1.0, DD ≤25%; death always fails); `report` (text/JSON), `gate`
  (exit 1 with named reasons), `track` (log-tailing console).
  Log enrichment that unlocked it: `OrderPlaced{price}`,
  `SL/TP{closed_units}` (agent-runtime + persist shapes).
  E2E: 300-tick scalper log → 34 trades, PF 0.58, gate FAILs with
  reasons (correct — random-walk scalping loses to spread).
  Known limits: P&L gross of commission (commissions live in equity
  metrics); resumed-run pre-log opens reconcile via snapshots; Sharpe
  unannualized (ticks have no clock).
  Verify: `cargo test -p analytics` (17) green.
- `P2-3` Real `licensing::check()` signature verification (Ed25519 offline).
- `P2-4` Venue adapters ✅ DONE (OANDA practice; more venues by demand).
  Landed: `broker-oanda` — market orders with signed-unit mapping,
  account mirror + genuine `reconcile()` (summary + openPositions +
  best-effort pricing; hedged positions refused loudly), redacted Debug,
  10s client timeout, `withdraw()` explicitly unsupported (splits defer
  via existing agent logic). Practice-only is mechanical:
  `is_practice_url()` allowlists the practice host (https, exact host —
  lookalikes fail); orchestrator live mode additionally requires
  `[oanda] account_id`, `OANDA_API_KEY` env (never config), distinct
  symbols per agent, and a successful opening reconcile per agent —
  ANY failure aborts the whole run (fail closed). Verified: trade host
  refused, missing account/key refused, dummy-key run reached the real
  API, parsed its auth error, and aborted exit 1.
  Demo procedure (needs user practice credentials — not runnable here):
  token in env → uncomment `[oanda]` template → `mode="live"` →
  `analytics gate` on the log → record stats before trusting anything.
  Ignored read-only practice test scaffolded (`-- --ignored`).
  Known limits: shared-account symbols must be distinct (sub-accounts
  follow-up); marks between reconciles are feed-provided (venue pricing
  feed is future work); second venue not started.
  Verify: `cargo test -p broker-oanda` (8 + 1 ignored) green.
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
