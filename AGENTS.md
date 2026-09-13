# AGENTS.md — TraderY — LLM ENTRYPOINT

> LLMs: read this file first. It is the index. Do not start coding until you have opened the 3 docs below relevant to your task.

## 1. What this is

Autonomous FX multi-agent paper-trading MVP in Rust (12 crates). Agents trade via `Broker` trait, die at zero, split at double, with SL/TP risk layer outside strategies. Paper-only today — `mode="live"` refuses to run. Crash resume via `--resume` (see `docs/PLAN.md:P1-1`).

## 2. Read in this order

1. `docs/CODEMAP.md` — file index, dataflow, function signatures, gotchas G1-G8. OPEN FIRST.
2. `docs/ANALYSIS.md` — what was verified broken vs fixed (A1-A5 carry FIXED/OPEN status). OPEN BEFORE EDITING.
3. `docs/PLAN.md` — P0 + P1-1 DONE (19 tests green); work P1-2 -> P2 in order. PICK ONE TASK.
4. `README.md` + `config.toml` — product story + runnable config (README now matches code: live refuses, paper margin/split semantics documented).

## 3. Quick commands

```bash
cargo build -p orchestrator
cargo test
cargo run -p orchestrator            # reads ./config.toml
cargo run -p orchestrator my-config.toml
cargo run -p orchestrator -- config.toml --resume  # rebuild from newest data/run-*.jsonl
```

Run output: console + `data/run-<timestamp>.jsonl` (append-only; snapshots every `snapshot_every_n_ticks` for `--resume`).

## 4. Rules for all agents

- Keep `core/src/lib.rs` trait shape. Add trait methods with defaults only.
- Risk layer (`agent-runtime/src/lib.rs:109 check_risk_exits`) runs before strategy — never bypass.
- P0 paper-honesty + P1-1 resume are DONE (19 tests: 9 broker-paper + 7 agent-runtime + 3 persistence). Work P1-2 -> P2 in order; no live broker, real feed, or analytics until P1 (inventory sizing + feeds) lands.
- Money is `f64` — do not migrate to decimal without a P2 task.
- After every edit: `cargo test` + `cargo build -p orchestrator`. Update `docs/CODEMAP.md` if behavior changed.

## 5. Where to edit (common tasks)

| Want to... | Open these only |
|---|---|
| Fix fills/margin | `broker-paper/src/lib.rs:44-148`, `core/src/lib.rs:102-130`, `docs/PLAN.md:P0-1` |
| Fix split/death | `agent-runtime/src/lib.rs:144-215`, `docs/PLAN.md:P0-2` |
| Fix live-gate | `orchestrator/src/main.rs:56-105`, `trading-config/src/lib.rs:35-46`, `docs/PLAN.md:P0-3` |
| Add tests | `broker-paper/src/lib.rs`, `agent-runtime/src/lib.rs`, `docs/PLAN.md:P0-4` |
| Crash recovery | `persistence/src/lib.rs:57`, `agent-runtime/src/lib.rs:100-104`, `docs/PLAN.md:P1-1` |
| SMA inventory | `strategy-sma/src/lib.rs:41-58`, `docs/PLAN.md:P1-2` |
| Feeds/lag | `orchestrator/src/main.rs:113-203`, `feed-mock/src/lib.rs`, `docs/PLAN.md:P1-3` |

## 6. Do NOT

- Do not add `broker-oanda` / real execution until P1 (replay + inventory sizing) lands.
- Do not treat `mode="live"` as paper-with-risk — it refuses to run (see `orchestrator/src/main.rs:58-62`).
- Do not assume `read_all()` runs on startup — only with `--resume`, and only snapshots/finals fold (ticks/orders are not replayed). Crash window = `snapshot_every_n_ticks`.
- Do not pyramid SMA (still inventory-blind, P1-2) — shorts now margin-checked, that part is fixed.
