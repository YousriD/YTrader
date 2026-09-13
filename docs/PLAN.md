# PLAN — TraderY — For LLM agents

> Goal: make paper mode honest BEFORE adding any live broker. Do not add `broker-oanda`, real news, or analytics until P0 is green. Work in order P0 -> P1 -> P2. Each task lists files to touch, exact acceptance criteria, and how to verify.

## Global rules (obey on every task)

- `R1` Keep trait shape in `core/src/lib.rs`. Add methods with default bodies so existing impls still compile (see `Broker::reconcile()` pattern at `core/src/lib.rs:119-121`).
- `R2` Risk stays outside strategy. Edit `agent-runtime/src/lib.rs:109` and `broker-paper/src/lib.rs`, never bypass `check_risk_exits()`.
- `R3` Money stays `f64` for P0/P1. Note rounding TODOs, do not migrate to decimal yet.
- `R4` Run `cargo test` and `cargo build -p orchestrator` after every task. No `cargo run` with `mode="live"`.
- `R5` Update `docs/CODEMAP.md:G1-G8` and `README.md` caveats when behavior changes.

## P0 — Honest paper (do first, blocks everything else)

### P0-1 Fix PaperBroker flip + margin
- Touch: `broker-paper/src/lib.rs:37-84`
- Do:
  1. Add `margin_required(order, price)` — at minimum reject `Sell` if `units * price > balance * max_leverage` (start `max_leverage=1.0` for spot-parity, make configurable).
  2. On flip (sign change with leftover): set `avg_entry_price = fill_price` for the leftover/new side. On partial close without flip: keep old avg. On flat: reset to 0.0.
  3. Add `withdraw(amount)` seam to `Broker` trait in `core/src/lib.rs:102` with default no-op, implement for real in `PaperBroker` (subtract from `balance`, error if insufficient).
- Accept: unit tests prove (a) long 1000 @1.10 with $50 rejects, (b) short 1000 with $50 rejects at 1x, (c) long 1000 -> sell 2000 leaves short priced at new fill, not old entry.
- Verify: `cargo test -p broker-paper`

### P0-2 Fix split to move money + stop loop
- Touch: `agent-runtime/src/lib.rs:194-200`, `core/src/lib.rs:102` (new `withdraw`), `orchestrator/src/main.rs:218-234` (print withdrawn vs equity)
- Do:
  1. Call `broker.withdraw(withdrawn)` on split. On failure, emit `OrderRejected` and do NOT advance `baseline`.
  2. Set `baseline = broker.account_state().equity` AFTER withdraw (not `equity/2` pre-withdraw).
  3. Ensure split fires at most once per crossing (require `equity >= baseline*2` with new post-withdraw baseline).
- Accept: test starts $100, forces equity $200, ticks once -> broker equity ~$100, `total_withdrawn` $100, next tick with flat price emits NO second split.
- Verify: `cargo test -p agent-runtime`

### P0-3 Gate live mode honestly
- Touch: `orchestrator/src/main.rs:56-60,72-105`, `trading-config/src/lib.rs:35-46`
- Do: if `mode="live"` and no `LiveBroker` feature/crate exists, `eprintln!` + `exit(1)` with "no live broker compiled in — refusing to run". Keep current skip-LLM behavior as secondary. Update `README.md:118-119` to say live is currently disabled, not risky.
- Accept: `mode="live"` exits non-zero with clear message; `mode="test"` unaffected.
- Verify: `cargo run -p orchestrator -- config.toml` still works; live config exits.

### P0-4 Tests for fill/SL-TP/death/split (the product logic)
- Touch: NEW `broker-paper/src/tests.rs` or inline `#[cfg(test)]`, NEW `agent-runtime/src/tests.rs`
- Do minimum 8 tests:
  1. buy-reject-insufficient, 2. sell-reject-no-margin, 3. add-to-position-weighted-avg, 4. partial-close-keeps-avg, 5. flip-resets-entry, 6. SL-fires-before-strategy, 7. TP-fires-before-strategy, 8. split-once-no-loop + death-at-zero.
- Accept: `cargo test` all green. This is the exit gate for P0.

## P1 — Crash safety + inventory-aware strategies (do second)

### P1-1 Replay/reconcile
- Touch: `persistence/src/lib.rs:57`, `agent-runtime/src/lib.rs:100-104`, `orchestrator/src/main.rs:68-105`
- Do: on startup, `read_all()` last `run-*.jsonl`, reconstruct `baseline/total_withdrawn/open_units` per agent, pass to `Agent::new()` or new `Agent::restore()`. For paper, restore in-memory; for future live, call `reconcile()` to overwrite with broker truth.
- Accept: kill -9 mid-run, restart with same log dir -> no duplicate position, baseline preserved. Test with synthetic log file.
- Verify: `cargo test -p persistence && cargo test -p agent-runtime`

### P1-2 Inventory-aware SMA
- Touch: `strategy-sma/src/lib.rs:41-58`
- Do: if `ctx.account.open_units != 0`, either return `None` (skip-pyramid) or emit flatten (opposite side, `units = abs(open_units)`) before reversing. Add `allow_pyramid: bool` + `max_units: f64` fields, wire from `trading-config/src/lib.rs:21-33` + `config.toml`.
- Accept: test with open long + bullish crossover -> no second buy when `allow_pyramid=false`.

### P1-3 Lag + feed fixes
- Touch: `orchestrator/src/main.rs:113-145,187-203`
- Do: count `Lagged(n)` per agent, emit `OrderRejected("tick-lag-skipped")` or log metric; change producer to per-symbol feeds (`HashMap<symbol, MockFeed>`) instead of `agents[0].symbol`.
- Accept: multi-symbol config fans out correctly; lag no longer silent.

## P2 — Sellable hardening (do last)

- `P2-1` Decimal money (`rust_decimal`), spread+commission config per symbol.
- `P2-2` Performance analytics from event log (Sharpe, max drawdown, win rate) — offline binary reading `read_all()`.
- `P2-3` Real `licensing::check()` signature verification (Ed25519 offline).
- `P2-4` `broker-oanda` crate implementing `Broker` + real `reconcile()` querying open positions.

## Explicit non-goals (do NOT do in P0/P1)

- No live trading, no real API keys, no HFT/market-making, no NautilusTrader port, no multi-process distribution.

## How to pick up work (LLM checklist)

1. Open `docs/CODEMAP.md`, then only the files listed in your P-task.
2. Open `docs/ANALYSIS.md:A1-A5` for bug context.
3. Implement smallest diff that meets Accept criteria.
4. Run `cargo test` + `cargo build -p orchestrator`. Paste failures, fix, repeat.
5. Update `docs/CODEMAP.md` gotchas if behavior changed.
