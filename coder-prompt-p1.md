# YTrader — Next Implementation Pass

## Status header (read first — do not re-derive this from the diff)

- **Step 0 — RESOLVED.** The README contradiction quoted below no longer
  exists: both sections agree, and `--resume` was verified end-to-end
  (fresh run → `--resume` run rebuilt all agents with prior balances).
  Run the Step 0 commands once to confirm, then move on. Do not
  "fix" documentation that already agrees.
- **Step 1 — DONE via snapshot-fold (not full event replay).** Crash
  recovery works: periodic `snapshot` records + full-state
  `final_summary`, `persistence::load_snapshots()`, `--resume` flag
  (19 tests green). This is the accepted design — do NOT build a
  second full-event-replay path. Ticks/orders are intentionally never
  replayed; crash window = `snapshot_every_n_ticks`. The one gap left:
  the restart proof is unit tests + manual e2e runs; Step 1 asks for an
  automated restart test — implement that if anything.
- **Steps 2–3 — DONE 2026-09-13 (40 tests green).** Registry
  (`strategy-indicators`: RSI, Donchian, AtrSizer, NewsGate + SMA guard),
  recursive config builder, per-symbol fan-out, lag accounting. Details
  in `docs/PLAN.md:P1-2/P1-3`. This prompt is now a historical record —
  work from `docs/PLAN.md` P2 next.

## Context

You're working on YTrader, a Rust workspace for a paper/live FX trading
system with a pool of independent agents (algorithmic + LLM-driven),
each with its own small stake, a die-at-zero / split-at-double
lifecycle, and an event-sourced audit log. Read `README.md` and
`docs/PLAN.md` in full before changing anything — they describe the
architecture and the priority-ordered roadmap (P0/P1/P2) this task
continues.

## Non-negotiable constraints — do not weaken these under any
## circumstance, even if a task below seems to require it

1. **`mode = "live"` must refuse to run unless a real (non-paper)
   broker adapter is wired in and its `reconcile()` is a genuine
   implementation, not the trait's no-op default.** If you're not
   implementing a real broker adapter in this pass, leave the existing
   hard-exit behavior exactly as is.
2. **No `llm`/hybrid-LLM strategy may ever be constructed when
   `mode = "live"`.** NOTE: live currently hard-exits before any agent
   is built, so the skip-LLM branch in the orchestrator is presently
   unreachable dead code. Keep it as defense-in-depth for the day a
   real broker adapter lands — do not delete it, and do not rely on it
   today. The gate in constraint #1 is what actually protects live.
3. **Stop-loss / take-profit checks must run before the strategy is
   consulted each tick**, independent of strategy output, exactly as
   `agent-runtime::Agent::on_tick` currently orders things. Don't
   reorder this for convenience.
4. **The split-at-double logic must only ever move real cash via
   `Broker::withdraw()`**, and must keep deferring (with a rejection
   notice, not silent skip) while any profit is unrealized. Do not
   revert to bookkeeping-only splits.
5. **Every new crate/module must go behind the existing trait
   boundaries** (`Broker`, `MarketFeed`, `NewsFeed`, `Strategy`) rather
   than special-casing a new concrete type through the orchestrator.
6. **Every state change must still be recorded through
   `persistence::EventLog`** — no new mutable state that bypasses the
   event log as source of truth.
7. **New `config.toml` fields must carry serde defaults** (the pattern
   `snapshot_every_n_ticks` established) so old configs still parse.
   New `StrategySpec` variants are safe by construction; new required
   fields are not — never add one.

If any task below seems to conflict with one of these constraints,
stop and flag the conflict in your PR description instead of silently
resolving it either way.

## Step 0 (verify only — already resolved, do not re-implement)

The contradiction quoted below was fixed and `--resume` verified
end-to-end. Confirm it is still fixed, nothing more:

The README's Quick Start section currently claims:

> restart with `--resume` to rebuild broker/baseline state from the
> newest log instead of fresh stakes

but the Architecture Decisions section says:

> the orchestrator does not replay it on startup yet, so a crash still
> restarts from fresh stakes

These cannot both be true. Determine which one is actually correct by
testing it:

```bash
cargo run -p orchestrator -- config.toml
# let it run a bit, place a few orders, then Ctrl+C
cargo run -p orchestrator -- config.toml --resume
# inspect: did balances/positions actually restore, or did it start fresh?
```

- If `--resume` does nothing (or doesn't exist as a real flag), fix the
  README to remove the false claim and make Step 1 below the actual
  implementation.
- If it partially works, describe exactly what it does and doesn't
  restore, and fix both README sections to say the same, accurate
  thing.

**Do not leave the two sections contradicting each other in your PR.**
(If they still agree, note that in one line and move on.)

## Step 1 (P1-1): DONE — only the automated restart test is left

Snapshot-based recovery is implemented and green. Do not rebuild it.
The single remaining item from the original acceptance list:

- Add an automated restart test: drive ticks, capture state, rebuild
  from the same log via `load_snapshots()` + `Agent::restore()`, assert
  reconstructed state matches. (Unit coverage of fold/restore exists;
  the e2e proof so far is manual runs.) Full event-by-event replay is
  explicitly out of scope — snapshots are the design.

## Step 2 (P1-2): Inventory-aware strategies / strategy registry

Goal, per `docs/PLAN.md:P1-2`: move from the current fixed `sma`/`llm`
strategy kinds toward a small registry of algorithmic strategies (RSI,
Donchian channel breakout, ATR-based position sizing, a simple
news-gate that suppresses entries around scheduled high-impact events)
that can be composed per agent, with the LLM strategy repositioned as
a regime router (decides *which* algorithmic strategy an agent should
be running right now) rather than a direct order-placer.

Acceptance criteria:
- New strategies (RSI, Donchian, ATR sizer) are separate crates or
  modules implementing the existing `Strategy` trait — no changes to
  the trait itself required for this. Prefer ONE new `strategy-indicators`
  crate over a crate per indicator.
- Every new strategy reads `ctx.account` and does NOT pyramid by
  default: flat-or-skip when a position is open unless the agent opts
  in (`allow_pyramid` + `max_units`), and sized to respect the broker's
  1x exposure margin. This inventory-awareness is the actual P1-2 goal —
  a second fixed-size crossover would not count as done.
- `trading-config::StrategySpec` gains variants for these, parseable
  from `config.toml`, following the existing `#[serde(tag = "kind")]`
  pattern.
- If you implement the "LLM as regime router" idea, it must still go
  through `strategy-hybrid` with an algorithmic fallback, and must
  still respect constraint #2 (never constructed in live mode).
- Explicitly do NOT implement any "whale tracking" or order-flow/tape
  reading for spot FX — there's no consolidated tape for retail spot
  FX, so this would be building on a false premise. If sentiment/flow
  proxies are wanted, use COT-report-style positioning data or news
  sentiment (already covered by `strategy-llm`/`news-mock`), not a
  fabricated order-flow feed.
- Add unit tests for each new strategy's decision logic against known
  synthetic price sequences (e.g., Donchian breakout should trigger on
  a specific constructed candle sequence, not trigger on a flat one).

## Step 3 (P1-3): Multi-symbol feed + lag accounting

Goal: remove the current assumption that every agent trades the same
single symbol (`config.agents[0].symbol` in the orchestrator), and
properly account for `broadcast::error::RecvError::Lagged(n)` instead
of silently continuing.

Acceptance criteria:
- The orchestrator groups agents by distinct `symbol` values from
  config and runs one feed subscription per symbol, fanning out only
  to the agents watching that symbol — not one global feed pretending
  every agent shares it.
- `MockFeed` (or its replacement) can run multiple independent
  instances concurrently, one per symbol, without shared mutable state
  between them.
- When an agent's tick receiver returns `Lagged(n)`, this is counted
  and surfaced (e.g., a `lagged_ticks` counter per agent visible in the
  final report and/or logged as its own event kind), not silently
  swallowed as it is today.
- Add a test or a documented manual test procedure showing two agents
  on different symbols receive independent price streams and don't
  cross-contaminate each other's history.

## Testing & documentation requirements (apply to every step above)

- `cargo build --workspace` and `cargo test --workspace` must pass
  before you consider a step done. Paste the actual test count/output
  in your PR description — don't just claim "tests pass."
- Update `README.md` and `docs/PLAN.md` to reflect exactly what's true
  after your change, no aspirational claims about features that are
  partially done. If something is partially done, say so explicitly
  and note what's left, the same way the existing PLAN.md priority
  tags do.
- Commit each step (0, 1, 2, 3) as its own commit or PR so it's
  reviewable independently — don't bundle all four into one giant diff.
- Do not touch the NautilusTrader decision, the licensing seam, or the
  `Broker::reconcile()` trait signature as part of this pass — those
  are settled for now and out of scope here.

## When you're done

Leave a short summary at the top of `docs/PLAN.md` (or wherever status
is tracked) listing which of Steps 0–3 are fully done, partially done,
or not attempted, and why, so the next review can pick up accurately
without re-deriving it from the diff.
