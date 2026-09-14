# YTrader — Standalone Dashboard (Slint + plotters, separate process)

## Context

Read `README.md` and `docs/PLAN.md` in full before touching anything.
This task adds a **second, independent binary** — a live dashboard —
alongside the existing `orchestrator`. It does not replace console
logging or the JSONL event log; it's an additional consumer of the
same live data.

## Non-negotiable constraints

1. **Two separate binaries, two separate processes.** The dashboard
   (`ytrader-gui` or similar crate name) must be able to crash, hang,
   or simply not be running at all, with zero effect on the trading
   engine (`orchestrator`). Do not merge them into one binary and do
   not have the orchestrator block on the GUI being connected.
2. **The GUI is read-only in this pass.** It displays live state; it
   must not be able to start, stop, reconfigure, or place orders for
   any agent. No control path from GUI to engine at all yet — that's
   explicitly out of scope here, to avoid rushing a write-path into
   the trading engine without proper safeguards.
3. **Do not touch existing safety-critical logic**: `mode = "live"`
   gating, the stop-loss/take-profit-before-strategy ordering in
   `agent-runtime`, the real-cash `Broker::withdraw()` split logic, or
   the `Broker::reconcile()` seam. This task only adds an observer.
4. **The orchestrator's own performance must not regress.** Streaming
   to the GUI must be best-effort / non-blocking — if the GUI isn't
   connected, or its socket buffer is full, the orchestrator does not
   stall waiting on it.
5. **Never show a fabricated or placeholder number as if it were a
   real computed stat.** Stats must be genuinely computed (see the
   stats panel section below) — if there isn't enough data yet for a
   given agent, show that fact explicitly rather than a "0.00" that
   looks like a real result.

## Architecture

```
orchestrator (existing binary)
   |
   |  same events it already logs to EventLog, ALSO published here:
   v
local IPC broadcast layer (new, small)
   |
   |  (Unix domain socket on Linux/macOS; loopback TCP on Windows)
   v
ytrader-gui (new binary)
   - Slint UI shell
   - plotters-rendered candlestick panel (image embedded in Slint)
   - live positions/holdings table
   - live event feed
```

### IPC layer

- Add a new small crate, e.g. `live-feed`, defining:
  - A `LiveEvent` enum/struct (can reuse or wrap the existing
    `persistence::EventRecord` shape — don't invent a second,
    diverging schema for the same data).
  - A `LiveFeedPublisher` the orchestrator holds and calls `publish()`
    on every time it currently calls `EventLog::append()` — same call
    site, two side effects.
  - A `LiveFeedSubscriber` the GUI process connects with.
- Transport: Unix domain socket (`/tmp/ytrader.sock` or under a proper
  runtime dir) on Linux/macOS, falling back to a loopback TCP port
  (e.g. `127.0.0.1:4770`) on Windows, behind a `cfg` or a small trait
  so the orchestrator code calling `publish()` doesn't care which.
- Framing: newline-delimited JSON is fine and keeps this consistent
  with how `EventLog` already serializes records — no need for a
  binary protocol here.
- The orchestrator must work identically with zero subscribers
  connected (this is the normal case when you're not looking at the
  dashboard) — publishing is fire-and-forget, dropped if nobody's
  listening.
- If the GUI connects mid-run, it should NOT expect full history over
  the socket — it gets live events from that point forward. For
  "what happened before I connected," it can separately read the
  current run's JSONL file once via `EventLog::read_all()` to backfill
  its in-memory state, then switch to live socket events. Implement
  this backfill-then-live-tail pattern explicitly, don't skip it.

### GUI (`ytrader-gui` crate)

- **Shell: Slint.** Use one of Slint's built-in widget styles
  (Fluent/Material/Cupertino) rather than building custom widgets from
  scratch for this first pass.
- **Licensing compliance (do this, it's a real legal requirement, not
  optional):** if you use Slint under the Royalty-free license, the
  app MUST display the `AboutSlint` widget (or the Slint badge)
  somewhere reachable from the main UI — an About screen/dialog is
  sufficient. Do not ship without this unless a Commercial license is
  separately obtained. Note clearly in the PR which license path was
  used.
- **Candlestick panel:** use `plotters` with its `CandleStick` series
  type, rendered to an in-memory bitmap, displayed as an image inside
  the Slint UI. Re-render on new candle data — a live feed at a few
  times per second is the right cadence, not per-tick, per-frame.
  - Include the SMA fast/slow lines as overlays where an agent's
    config uses `strategy = "sma"`, since that data is already
    available and makes the chart meaningfully more useful than a
    bare price line.
  - If `plotters`' default rendering looks dated once you see it
    running, `charts-rs` (also has a native `Candlestick` series,
    renders to SVG/PNG/WEBP) is an acceptable substitute — same
    "render to image, embed in Slint" pattern. Don't block on this;
    ship with `plotters` first, swap later if needed.
- **Live positions/holdings table:** one row per agent, columns:
  agent id, symbol, side (long/flat/short), units, entry price,
  current price, unrealized P&L, balance, equity, total withdrawn,
  status (alive/dead). Source this from `AccountState` plus the
  agent-runtime fields already exposed (`total_withdrawn`, `status()`).
- **Live event feed:** scrolling list of the most recent N events
  (orders placed/rejected, stop-loss/take-profit hits, splits, deaths),
  newest first, same data already flowing into `EventLog`.
- **Stats panel — computed client-side in the GUI, not the engine:**
  do NOT depend on any backend stats crate/field shape (we don't
  reliably know what may or may not already exist there locally).
  Instead, derive these directly in `ytrader-gui` from the same event
  stream it already consumes (backfill via `EventLog::read_all()` +
  live tail):
  - **Equity curve / max drawdown**: buffer equity values over time
    per agent (from `tick`/`order_placed`/etc. events that carry
    equity, or by tracking balance + open position value as events
    arrive) and compute running peak-to-trough drawdown from that
    buffer.
  - **Win rate / profit factor**: reconstruct closed trades by pairing
    entry and closing orders per agent (a position goes from nonzero
    `open_units` to zero, or flips sign) and classify each closed
    trade as win/loss from the realized P&L at that point.
  - **Sharpe ratio**: compute from the same equity buffer's period
    returns (simple daily/tick-bucketed returns are fine for a first
    pass — note the bucketing choice in a code comment since it
    affects the number).
  - **Critical UI rule:** distinguish "not enough data yet" from an
    actual computed zero. E.g. render "—" or "insufficient history"
    until an agent has closed at least a handful of trades, rather
    than showing "0.00" — a real 0.00 Sharpe and "we haven't computed
    this yet" are different facts and must not look identical on a
    financial dashboard.
  - This means the panel ships in this pass, with real (if
    client-computed, first-pass) numbers — not placeholders, and not
    deferred to a future backend implementation.

## Testing & documentation requirements

- The IPC layer needs at least one integration test: start a
  publisher, connect a subscriber, assert events arrive in order and
  that a slow/absent subscriber doesn't block the publisher (e.g. spin
  up the publisher, don't connect anything, assert `publish()` calls
  don't hang or error the orchestrator's own test suite).
- Manually verify the "GUI not running" case: run the orchestrator
  alone with no GUI attached, confirm behavior (throughput, console
  output, event log) is unchanged from before this task.
- Manually verify the "GUI crashes mid-run" case: start both, kill the
  GUI process, confirm the orchestrator keeps running normally.
- Update `README.md` with a new section describing the two-binary
  architecture, how to run each (`cargo run -p orchestrator`,
  `cargo run -p ytrader-gui`), the Slint license path used, and a
  short note on how the client-side stats are computed (equity buffer
  window, return-bucketing choice for Sharpe, trade-pairing logic for
  win rate) so the numbers are auditable rather than a black box.
- `cargo build --workspace` and `cargo test --workspace` must pass;
  paste actual output in the PR description. Add at least one test
  for the stats computation using a small synthetic sequence of
  orders/equity values with a hand-verified expected drawdown/win
  rate, so the math is checked, not just "it runs."

## Out of scope for this pass (don't do these)

- Any control path (start/stop/reconfigure agents) from the GUI.
- Depending on, or requiring the existence of, any backend stats
  crate — the GUI computes its own stats as described above.
- Multi-symbol-aware chart layout (fine to assume one chart panel per
  distinct symbol currently configured; don't over-engineer for
  symbols that don't exist yet if P1-3 multi-symbol work hasn't landed
  in this branch).
- Remote/networked access to the dashboard (loopback only, this
  machine only, for now).
