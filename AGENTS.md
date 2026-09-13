# AGENTS.md — TraderY — LLM ENTRYPOINT

> LLMs: read this file first. It is the index. Do not start coding until you have opened the 3 docs below relevant to your task.

## 1. What this is

Autonomous FX multi-agent paper-trading MVP in Rust (12 crates). Agents trade via `Broker` trait, die at zero, split at double, with SL/TP risk layer outside strategies. Paper-only today — `mode="live"` refuses to run (see `docs/ANALYSIS.md:A1`, fixed per `docs/PLAN.md:P0-3`).

## 2. Read in this order

1. `docs/CODEMAP.md` — file index, dataflow, function signatures, gotchas G1-G8. OPEN FIRST.
2. `docs/ANALYSIS.md` — verified bugs with file:line evidence (A1-A5). OPEN BEFORE EDITING.
3. `docs/PLAN.md` — ordered tasks P0->P1->P2 with accept criteria. PICK ONE TASK, DO NOT SKIP P0.
4. `README.md` + `config.toml` — product story + runnable config (note README overclaims live risk).

## 3. Quick commands

```bash
cargo build -p orchestrator
cargo test
cargo run -p orchestrator            # reads ./config.toml
cargo run -p orchestrator my-config.toml
```

Run output: console + `data/run-<timestamp>.jsonl` (append-only, never replayed today).

## 4. Rules for all agents

- Keep `core/src/lib.rs` trait shape. Add trait methods with defaults only.
- Risk layer (`agent-runtime/src/lib.rs:109 check_risk_exits`) runs before strategy — never bypass.
- Paper honesty first: fix broker margin/flip + split-withdraw + live-gate + tests (`docs/PLAN.md:P0`) before any live broker, real feed, or analytics.
- Money is `f64` — do not migrate to decimal without a P2 task.
- After every edit: `cargo test` + `cargo build -p orchestrator`. Update `docs/CODEMAP.md` if behavior changed.

## 5. Where to edit (common tasks)

| Want to... | Open these only |
|---|---|
| Fix fills/margin | `broker-paper/src/lib.rs:37-84`, `core/src/lib.rs:102-122`, `docs/PLAN.md:P0-1` |
| Fix split/death | `agent-runtime/src/lib.rs:144-203`, `docs/PLAN.md:P0-2` |
| Fix live-gate | `orchestrator/src/main.rs:56-105`, `trading-config/src/lib.rs:35-46`, `docs/PLAN.md:P0-3` |
| Add tests | `broker-paper/src/lib.rs`, `agent-runtime/src/lib.rs`, `docs/PLAN.md:P0-4` |
| Crash recovery | `persistence/src/lib.rs:57`, `agent-runtime/src/lib.rs:100-104`, `docs/PLAN.md:P1-1` |
| SMA inventory | `strategy-sma/src/lib.rs:41-58`, `docs/PLAN.md:P1-2` |
| Feeds/lag | `orchestrator/src/main.rs:113-203`, `feed-mock/src/lib.rs`, `docs/PLAN.md:P1-3` |

## 6. Do NOT

- Do not add `broker-oanda` / real execution until P0 tests green.
- Do not trust `README.md:118-119` live warning — live is currently paper (see ANALYSIS A1).
- Do not assume `Split` moved money or `read_all()` runs on startup — both false today.
- Do not pyramid SMA or short without margin check — bugs, not features.
