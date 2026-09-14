# PLAN — TraderY — For LLM agents

> Goal: make paper mode honest BEFORE adding any live broker. Do not add `broker-oanda`, real news, or analytics until P0 is green. Work in order P0 -> P1 -> P2. Each task lists files to touch, exact acceptance criteria, and how to verify.
>
> STATUS 2026-09-14: P0 + P1 + P2-1-venue + P2-2 + P2-4 + P2-5 + P2-6 + P2-7 DONE (107 tests). MT5 bridge path live (untested against a real terminal — yours is the first). Left: P2-3 licensing when selling nears; follow-ups below.

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
- `P2-5` World-info feeds ✅ DONE (calendar + gate; headline APIs follow-up).
  Landed: `news-calendar` — free keyless ForexFactory weekly JSON →
  `NewsFeed` (event-time items, sentiment always None by honesty rule),
  impact ordering with unknown→High fail-closed, hourly cache with
  dedup, death degrades to silence; `CalendarGate` decorator in
  `strategy-indicators` (owns feed, throttled 50-tick refresh,
  currencies from agent symbol, ±window suppression of High/Holiday).
  Config `calendar_gated{inner, window_minutes}` (recursive, LLM-gated
  at depth); builder arm + `contains_llm` updated.
  E2E (real egress here): ignored live-fetch test passes (0.22s);
  live 15-tick run with calendar-gated Donchian fetched the real week
  and traded through an empty window correctly.
  Open follow-up: headline APIs (RSS/Finnhub-style, key-gated) with LLM
  sentiment scoring behind the `strategy-llm` pattern; gate-suppression
  event kind for log visibility.
  Verify: `cargo test -p news-calendar` (5 + 1 ignored) + indicators (18) green.
- `P2-6` Tier 2 regime router ✅ DONE (works with AND without LLMs).
  Landed: `strategy-router` — `RouterBrain` trait with `RuleRouter`
  (deterministic drift/vol classifier, the DEFAULT brain: no key, no
  network) and `LlmRouter` (regime pick with threshold→cooldown, 8s
  bound); `RouterStrategy` chains LLM pick (validated) → rule pick →
  default, warm-keeps all candidates, reports `active_strategy()`.
  Core trait gained the `active_strategy()` default hook; Hybrid
  delegates to the deciding side; Agent emits `RegimeSelected` on change
  (persisted as `regime_selected` for attribution). Config
  `router{candidates, default, trending, ranging, trend_window, llm?}`
  with `validate()` (dangling names skip loudly) + `requires_llm_key()`
  (router never requires — degrades; live forces the rule brain and
  router-with-llm specs stay skipped in live). Builder Hybrid-wraps
  every router with a rebuilt-default fallback.
  E2E (no key): 40-tick rule-router run switched mr/tr on regime with
  `🧭 regime →` lines and candidate orders. Known behavior: classifier
  is twitchy on random-walk noise (switches every few ticks) — switch
  debounce/hysteresis is the follow-up, not a defect.
   Verify: router (9) + agent regime (2) + config (2) + builder (2) green.
- `P2-7` MT5 bridge path ✅ DONE (code complete; first live-terminal run is yours).
  Landed: `broker-mt5` — localhost bridge client, units↔lots conversion
  via cached contract spec (step-rounded DOWN, below-minimum rejected
  without HTTP), mirror + VWAP, DEMO-only reconcile (non-demo and dead
  terminals fail closed), `is_local_bridge_url()` (remote URLs refused:
  plaintext orders), `withdraw()` unsupported (splits defer).
  `mt5-sidecar/` + `bridge-mt5/YTraderNativeBridge.mq5` — native Rust/MT5
  polling bridge; no Python package required. The EA refuses non-demo accounts
  and defaults to observation-only until its `EnableOrders` input is enabled.
  6 routes mirroring the Rust client, filling-mode aware, magic-tagged
  orders; reviewed but NEVER run against a real terminal here.
  Config `[mt5]` (no credentials by design) + `live_venue = "mt5"` +
  per-agent `venue_symbol`; orchestrator dispatches venues via pure
  `resolve_live_venue()` (unit-tested refusal branches — also works
  around machines where the main binary can't execute).
  E2E here: mock-bridge tests (7 incl. reconcile/order/minimum/error
  paths) + gate tests; binary-e2e blocked by an OS Application Control
  policy on this box (documented below). On your machine:
  `start-mt5-demo.bat` (opens the Rust sidecar, waits for native-EA health,
  then runs `demo-mt5.toml` with full console).
  Open: first real terminal run (your MT5 demo), bridge live-fire fixes,
  sub-accounts, venue pricing feed.
  Verify: `cargo test -p broker-mt5` (8) green.

## Known environment issue (this dev box, 2026-09-14)

`target/debugorchestrator.exe` is blocked by an OS Application Control
policy (os error 4551) — rebuilds, renames, and `cargo run` all fail,
while test binaries execute fine. Unit/integration tests are therefore
the verification path here; binary e2e runs happen on your machine.
If you hit this too: allowlist the `target/` dir in Windows Security /
WDAC, or run from an unrestricted path.

## Explicit non-goals (do NOT do in P0/P1)

- No live trading, no real API keys, no HFT/market-making, no NautilusTrader port, no multi-process distribution.

## How to pick up work (LLM checklist)

1. Open `docs/CODEMAP.md`, then only the files listed in your P-task.
2. Open `docs/ANALYSIS.md:A1-A5` for bug context.
3. Implement smallest diff that meets Accept criteria.
4. Run `cargo test` + `cargo build -p orchestrator`. Paste failures, fix, repeat.
5. Update `docs/CODEMAP.md` gotchas if behavior changed.
