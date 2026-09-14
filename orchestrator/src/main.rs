use agent_runtime::{Agent, AgentEvent, AgentStatus};
use broker_oanda::{is_practice_url, OandaBroker};
use broker_paper::PaperBroker;
use chrono::Utc;
use feed_mock::MockFeed;
use news_mock::MockNewsFeed;
use persistence::{latest_run_file, load_snapshots, EventLog, EventRecord, Snapshot};
use rand::Rng;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use strategy_hybrid::HybridStrategy;
use strategy_indicators::{AtrSizer, CalendarGate, DonchianBreakout, NewsGate, Rsi};
use strategy_llm::LlmStrategy;
use strategy_router::{LlmRouter, RouterStrategy, RuleRouter};
use strategy_sma::SmaCrossover;
use tokio::sync::{broadcast, mpsc};
use trading_config::{AgentSpec, Mode, RunConfig, StrategySpec};
use trading_core::{Broker, MarketFeed, NewsFeed, NewsItem};

#[derive(Clone)]
struct TickMsg {
    tick: u32,
    candle: trading_core::Candle,
    news: Option<NewsItem>,
}

enum LogMsg {
    Event { agent_id: String, tick: u32, event: AgentEvent },
    /// Periodic restorable state (see P1-1). Written every
    /// `snapshot_every_n_ticks`; crash window = up to N ticks.
    Snapshot {
        agent_id: String,
        tick: u32,
        balance: f64,
        open_units: f64,
        entry_price: Option<f64>,
        baseline: f64,
        withdrawn: f64,
    },
    /// A tick broadcast this agent missed (P1-3). Visible in the final
    /// report and persisted — lag is never silent anymore.
    TickLag { agent_id: String, tick: u32, skipped: u64 },
    Final {
        agent_id: String,
        status: AgentStatus,
        equity: f64,
        withdrawn: f64,
        balance: f64,
        open_units: f64,
        entry_price: Option<f64>,
        baseline: f64,
        lag_incidents: u64,
        lag_ticks: u64,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // --- Licensing seam (no-op today; real check goes here later) ---
    match licensing::check() {
        licensing::LicenseStatus::Unlicensed => {
            println!("(running unlicensed — fine for personal use)");
        }
        licensing::LicenseStatus::Valid { owner } => {
            println!("Licensed to: {owner}");
        }
        licensing::LicenseStatus::Invalid { reason } => {
            eprintln!("License invalid: {reason}");
            std::process::exit(1);
        }
    }

    // --- Load config (agents are defined in config.toml, not code) ---
    // Usage: orchestrator [config.toml] [--resume]
    // --resume rebuilds agents from the newest data/run-*.jsonl instead
    // of fresh stakes. Without it, every run starts fresh.
    let cli_args: Vec<String> = std::env::args().skip(1).collect();
    let resume = cli_args.iter().any(|a| a == "--resume");
    let config_path = cli_args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| "config.toml".to_string());
    let config = match RunConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config from {config_path}: {e}");
            std::process::exit(1);
        }
    };

    println!("=== Autonomous FX Trading System — mode: {:?} ===\n", config.mode);
    let is_live = matches!(config.mode, Mode::Live);
    // P2-4 live gate: the OANDA practice host ONLY. Anything else —
    // real-money host, http, lookalikes, missing account/key — refuses
    // EXACTLY like before. Credentials never touch the config file.
    let oanda_creds: Option<(String, String)> = if is_live {
        if !is_practice_url(&config.oanda.base_url) {
            eprintln!("LIVE mode refused: base_url is not the OANDA practice host.");
            eprintln!("Got: {}", config.oanda.base_url);
            eprintln!("Live runs against practice ONLY (https://api-fxpractice.oanda.com).");
            std::process::exit(1);
        }
        if config.oanda.account_id.is_empty() {
            eprintln!("LIVE mode refused: [oanda] account_id is not set (see demo template in config.toml).");
            std::process::exit(1);
        }
        let api_key = match std::env::var("OANDA_API_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => {
                eprintln!("LIVE mode refused: OANDA_API_KEY env is not set.");
                std::process::exit(1);
            }
        };
        // One OANDA account merges same-symbol positions across agents,
        // which would corrupt per-agent accounting: require distinct
        // symbols (sub-accounts per agent is the follow-up).
        if symbols_of(&config.agents).len() != config.agents.len() {
            eprintln!("LIVE mode refused: agents share symbols on one OANDA account.");
            eprintln!("Give each live agent its own symbol (or its own sub-account per broker-oanda docs).");
            std::process::exit(1);
        }
        println!("LIVE mode: OANDA practice adapter (reconciled, fail-closed).\n");
        Some((api_key, config.oanda.account_id.clone()))
    } else {
        None
    };

    // --- Crash recovery (P1-1): opt-in resume from the newest log ---
    // Without --resume every run starts fresh (current behavior).
    // With --resume, per-agent broker/baseline/withdrawn state is rebuilt
    // from the latest snapshot/final_summary; dead agents stay dead and
    // start fresh. Strategy internals (SMA memory) rebuild over ticks.
    // NOTE: this MUST run before the new event log file is created below,
    // or "latest" would be the empty file of this very run.
    let snapshots: HashMap<String, Snapshot> = if resume {
        match latest_run_file("data") {
            Some(path) => match load_snapshots(&path) {
                Ok(map) => {
                    println!("Resuming from {} ({} agent(s) with state)\n", path.display(), map.len());
                    map
                }
                Err(e) => {
                    eprintln!("Cannot read {}: {e} — starting fresh.", path.display());
                    HashMap::new()
                }
            },
            None => {
                println!("--resume given but data/ has no runs yet — starting fresh.\n");
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    // --- Durable event log ---
    let run_id = Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let log_path = format!("data/run-{run_id}.jsonl");
    let event_log = Arc::new(EventLog::open(&log_path).expect("failed to open event log"));
    println!("Logging events to {log_path}\n");

    // --- Build agents from config ---
    let mut rng = rand::thread_rng();
    let mut agents: Vec<Agent> = Vec::new();

    for spec in &config.agents {
        // Live protection: practice runs DO reach this loop, so any
        // LLM anywhere in a strategy tree is skipped here, at any
        // nesting depth. Keyless agents are handled just below.
        if contains_llm(&spec.strategy) && is_live {
            println!("Skipping '{}' — LLM-driven agents are not permitted in live mode.", spec.id);
            continue;
        }
        // Hard requirements first: an agent that NEEDS an LLM key
        // (bare Llm, or LLM nested under sizing/gates — but NOT a
        // router, which degrades to its rule brain) is skipped keyless.
        // Then structural validation (router mappings must resolve).
        if spec.strategy.requires_llm_key() && std::env::var("ANTHROPIC_API_KEY").is_err() {
            println!("Skipping '{}' — ANTHROPIC_API_KEY not set.", spec.id);
            continue;
        }
        if let Err(e) = spec.strategy.validate() {
            eprintln!("Skipping '{}' — invalid strategy spec: {e}.", spec.id);
            continue;
        }

        let stake: f64 = if spec.stake_max > spec.stake_min {
            rng.gen_range(spec.stake_min..spec.stake_max)
        } else {
            spec.stake_min // fixed stake (min == max) must not panic gen_range
        };
        let max_units = spec.max_position_units.unwrap_or(spec.units);
        let llm_key = std::env::var("ANTHROPIC_API_KEY").ok();
        // Live mode never constructs LLM brains: pass None so routers
        // (and only routers — bare Llm agents are skipped above) run
        // their deterministic rule brain. Defense-in-depth, explicit.
        let llm_key_live_aware = if is_live { None } else { llm_key.as_deref() };
        let strategy = build_strategy(
            &spec.strategy,
            &spec.id,
            spec.units,
            spec.allow_pyramid,
            max_units,
            llm_key_live_aware,
        );

        let mut agent = match snapshots.get(&spec.id) {
            // Paper resume path (test mode only — live always reconciles).
            Some(snap) if oanda_creds.is_none() => {
                match PaperBroker::restore(snap.balance, snap.open_units, snap.entry_price.unwrap_or(0.0)) {
                    Ok(broker) => {
                        println!(
                            "Resuming '{}' — balance=${:.2} open_units={} baseline=${:.2} withdrawn_total=${:.2}",
                            spec.id, snap.balance, snap.open_units, snap.baseline, snap.withdrawn
                        );
                        Agent::restore(
                            spec.id.clone(),
                            spec.symbol.clone(),
                            strategy,
                            Box::new(apply_venue_economics(broker, spec)),
                            snap.baseline,
                            snap.withdrawn,
                        )
                    }
                    Err(e) => {
                        eprintln!(
                            "Snapshot for '{}' corrupt ({e}) — starting fresh with ${stake:.2}.",
                            spec.id
                        );
                        Agent::new(spec.id.clone(), spec.symbol.clone(), strategy, Box::new(apply_venue_economics(PaperBroker::new(stake), spec)), stake)
                    }
                }
            }
            _ => {
                // LIVE path: venue truth via fail-closed reconcile.
                // Snapshots are ignored here — the account is truth, and
                // a stale snapshot must never override it.
                if let Some((api_key, account_id)) = &oanda_creds {
                    if snapshots.contains_key(&spec.id) {
                        println!("LIVE '{}': ignoring old snapshot — venue reconcile is truth.", spec.id);
                    }
                    let mut ob = OandaBroker::new(
                        config.oanda.base_url.clone(),
                        api_key.clone(),
                        account_id.clone(),
                        spec.symbol.clone(),
                    );
                    match ob.reconcile().await {
                        Ok(()) => {
                            let st = ob.account_state();
                            println!(
                                "LIVE '{}': reconciled — balance=${:.2} open_units={}",
                                spec.id, st.balance, st.open_units
                            );
                            let baseline = st.equity;
                            Agent::new(spec.id.clone(), spec.symbol.clone(), strategy, Box::new(ob), baseline)
                        }
                        Err(e) => {
                            eprintln!("LIVE '{}': opening reconcile failed ({e}) — aborting run (fail closed).", spec.id);
                            std::process::exit(1);
                        }
                    }
                } else {
                    Agent::new(spec.id.clone(), spec.symbol.clone(), strategy, Box::new(apply_venue_economics(PaperBroker::new(stake), spec)), stake)
                }
            }
        };
        if let Some(sl) = spec.stop_loss_pct {
            agent = agent.with_stop_loss(sl);
        }
        if let Some(tp) = spec.take_profit_pct {
            agent = agent.with_take_profit(tp);
        }
        agent.reconcile().await; // no-op for paper, required seam for live
        agents.push(agent);
    }

    if agents.is_empty() {
        println!("No agents to run — check config.toml.");
        return;
    }

    // --- Wire up per-symbol broadcast (P1-3) + logging pipeline ---
    // One feed subscription per distinct symbol; agents only receive
    // their own symbol's walk. Same-symbol agents share one feed
    // instance (same walk); different symbols never cross-contaminate.
    // News is still fanned out globally (documented limitation: the mock
    // news has no symbol attribution; a real calendar feed would).
    let mut symbol_txs: HashMap<String, broadcast::Sender<TickMsg>> = HashMap::new();
    let mut symbol_feeds: HashMap<String, MockFeed> = HashMap::new();
    for symbol in symbols_of(&config.agents) {
        let (tick_tx, _) = broadcast::channel::<TickMsg>(1024);
        symbol_txs.insert(symbol.to_string(), tick_tx);
        symbol_feeds.insert(symbol.to_string(), MockFeed::new(1.1000, 0.0006));
    }
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogMsg>();
    let alive_count = Arc::new(AtomicUsize::new(agents.len()));

    let mut handles = Vec::new();
    let snapshot_every = config.snapshot_every_n_ticks;
    for agent in agents {
        let mut rx = symbol_txs
            .get(&agent.symbol)
            .expect("feed channel missing for agent symbol")
            .subscribe();
        let log_tx = log_tx.clone();
        let alive_count = alive_count.clone();
        let handle = tokio::spawn(async move {
            let mut agent = agent;
            let agent_id = agent.id.clone();
            let mut last_tick: u32 = 0;
            let mut lag_incidents: u64 = 0;
            let mut lag_ticks: u64 = 0;
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        last_tick = msg.tick;
                        if agent.status() == AgentStatus::Dead {
                            continue;
                        }
                        if let Some(news) = msg.news {
                            agent.push_news(news);
                        }
                        let events = agent.on_tick(msg.candle).await;
                        for event in events {
                            let just_died = matches!(event, AgentEvent::Died { .. });
                            let _ = log_tx.send(LogMsg::Event { agent_id: agent_id.clone(), tick: msg.tick, event });
                            if just_died {
                                alive_count.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        // Periodic restorable snapshot (P1-1). Dead agents
                        // stop here via `continue` above, so only the live
                        // ones checkpoint; death is recorded via died/final.
                        if snapshot_every > 0 && msg.tick % snapshot_every == 0 {
                            let st = agent.account_state();
                            let _ = log_tx.send(LogMsg::Snapshot {
                                agent_id: agent_id.clone(),
                                tick: msg.tick,
                                balance: st.balance,
                                open_units: st.open_units,
                                entry_price: st.entry_price,
                                baseline: agent.baseline(),
                                withdrawn: agent.total_withdrawn,
                            });
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        lag_incidents += 1;
                        lag_ticks += skipped;
                        let _ = log_tx.send(LogMsg::TickLag {
                            agent_id: agent_id.clone(),
                            tick: last_tick,
                            skipped,
                        });
                        continue;
                    }
                }
            }
            let state = agent.account_state();
            let _ = log_tx.send(LogMsg::Final {
                agent_id: agent_id.clone(),
                status: agent.status(),
                equity: state.equity,
                withdrawn: agent.total_withdrawn,
                balance: state.balance,
                open_units: state.open_units,
                entry_price: state.entry_price,
                baseline: agent.baseline(),
                lag_incidents,
                lag_ticks,
            });
        });
        handles.push(handle);
    }
    drop(log_tx);

    // --- Logger task: prints AND persists every event concurrently ---
    let logger_run_id = run_id.clone();
    let logger_event_log = event_log.clone();
    let logger = tokio::spawn(async move {
        let mut finals = Vec::new();
        while let Some(msg) = log_rx.recv().await {
            match msg {
                LogMsg::Event { agent_id, tick, event } => {
                    print_event(&agent_id, tick, &event);
                    persist_event(&logger_event_log, &logger_run_id, &agent_id, tick, &event);
                }
                LogMsg::Snapshot { agent_id, tick, balance, open_units, entry_price, baseline, withdrawn } => {
                    let snap = Snapshot { balance, open_units, entry_price, baseline, withdrawn };
                    let _ = logger_event_log.append(&persistence::snapshot_record(&logger_run_id, &agent_id, tick, &snap));
                }
                LogMsg::TickLag { agent_id, tick, skipped } => {
                    println!("[t{tick}] {agent_id} tick-lag: skipped {skipped} tick(s) (slow consumer)");
                    let record = EventRecord {
                        ts: Utc::now(),
                        run_id: logger_run_id.clone(),
                        agent_id: agent_id.clone(),
                        tick,
                        kind: "tick_lag".to_string(),
                        data: serde_json::json!({ "skipped": skipped }),
                    };
                    let _ = logger_event_log.append(&record);
                }
                LogMsg::Final { agent_id, status, equity, withdrawn, balance, open_units, entry_price, baseline, lag_incidents, lag_ticks } => {
                    let snap = Snapshot { balance, open_units, entry_price, baseline, withdrawn };
                    let record = persistence::final_record(&logger_run_id, &agent_id, &format!("{status:?}"), equity, &snap);
                    let _ = logger_event_log.append(&record);
                    finals.push((agent_id, status, equity, withdrawn, lag_incidents, lag_ticks));
                }
            }
        }
        finals
    });

    // --- Producer: one mock walk per symbol + global news, fanned out ---
    let mut news_feed = MockNewsFeed::new(config.news_every_n_ticks);

    for tick in 0..config.ticks {
        if alive_count.load(Ordering::SeqCst) == 0 {
            println!("\nAll agents dead. Stopping early at tick {tick}.");
            break;
        }
        let news = news_feed.next_headline().await;
        for (symbol, tx) in &symbol_txs {
            let feed = symbol_feeds.get_mut(symbol).expect("feed missing for symbol");
            let candle = match feed.next_price(symbol).await {
                Some(c) => c,
                None => continue,
            };
            let _ = tx.send(TickMsg { tick, candle, news: news.clone() });
        }
        tokio::time::sleep(Duration::from_millis(config.tick_delay_ms)).await;
    }
    drop(symbol_txs); // closes the channels; agent tasks exit their loops

    for handle in handles {
        let _ = handle.await;
    }
    let finals = logger.await.unwrap_or_default();

    println!("\n=== Final report ===");
    for (id, status, equity, withdrawn, lag_incidents, lag_ticks) in finals {
        println!("{id:<16} status={status:?} equity=${equity:>8.2} withdrawn_total=${withdrawn:.2} lagged={lag_incidents}/{lag_ticks}");
    }
    println!("\nFull event log: {log_path}");
}

/// Apply per-agent venue economics (P2-1) to a fresh or restored broker.
/// Called on BOTH paths so a resumed agent trades under the same
/// rulebook as a new one — economics come from config, positions from
/// the snapshot.
fn apply_venue_economics(mut broker: PaperBroker, spec: &AgentSpec) -> PaperBroker {
    if let Some(min_units) = spec.min_units {
        broker = broker.with_min_units(min_units);
    }
    if let Some(min_notional) = spec.min_notional {
        broker = broker.with_min_notional(min_notional);
    }
    if spec.commission_per_unit > 0.0 {
        broker = broker.with_commission_per_unit(spec.commission_per_unit);
    }
    if let Some(pips) = spec.spread_pips {
        broker = broker.with_spread_pips(pips);
    }
    broker
}

/// True if an LLM strategy sits anywhere in the (possibly nested)
/// strategy tree. Used for keyless/live gating at any depth.
fn contains_llm(spec: &StrategySpec) -> bool {
    match spec {
        StrategySpec::Llm { .. } => true,
        // A router WITH an llm section counts as LLM-containing (skipped
        // in live): no silent brain-swaps with money — remove the section
        // to explicitly choose the rule brain. A router without one is
        // pure code and runs anywhere.
        StrategySpec::Router { llm, .. } => llm.is_some(),
        StrategySpec::Atr { inner, .. }
        | StrategySpec::NewsGated { inner, .. }
        | StrategySpec::CalendarGated { inner, .. } => contains_llm(inner),
        _ => false,
    }
}

/// Distinct agent symbols in first-seen config order (P1-3 fan-out key:
/// one feed subscription per symbol, not one global feed).
fn symbols_of(agents: &[AgentSpec]) -> Vec<&str> {
    let mut out = Vec::new();
    for spec in agents {
        if !out.contains(&spec.symbol.as_str()) {
            out.push(spec.symbol.as_str());
        }
    }
    out
}

/// Recursive strategy builder (P1-2): every `StrategySpec` kind,
/// including nested `atr` / `news_gated` wrappers, becomes a boxed
/// `Strategy`. Inventory knobs apply to every leaf. `llm_key` must be
/// `Some` when the tree contains `Llm` — the caller skips keyless LLM
/// agents before calling, so the expect below is infallible in practice.
fn build_strategy(
    spec: &StrategySpec,
    name: &str,
    units: f64,
    allow_pyramid: bool,
    max_units: f64,
    llm_key: Option<&str>,
) -> Box<dyn trading_core::Strategy> {
    match spec {
        StrategySpec::Sma { fast, slow } => Box::new(
            SmaCrossover::new(name, *fast, *slow, units).with_inventory(allow_pyramid, max_units),
        ),
        StrategySpec::Rsi { period, overbought, oversold } => Box::new(
            Rsi::new(name, *period, *overbought, *oversold, units).with_inventory(allow_pyramid, max_units),
        ),
        StrategySpec::Donchian { channel } => Box::new(
            DonchianBreakout::new(name, *channel, units).with_inventory(allow_pyramid, max_units),
        ),
        StrategySpec::Atr { inner, period, risk_pct, max_units: cap } => Box::new(AtrSizer::new(
            build_strategy(inner, name, units, allow_pyramid, max_units, llm_key),
            name,
            *period,
            *risk_pct,
            *cap,
        )),
        StrategySpec::NewsGated { inner, cooldown_ticks, sentiment_threshold } => Box::new(NewsGate::new(
            build_strategy(inner, name, units, allow_pyramid, max_units, llm_key),
            name,
            *cooldown_ticks,
            *sentiment_threshold,
        )),
        StrategySpec::CalendarGated { inner, window_minutes } => Box::new(CalendarGate::new(
            build_strategy(inner, name, units, allow_pyramid, max_units, llm_key),
            name,
            *window_minutes,
        )),
        StrategySpec::Router { candidates, default, trending, ranging, trend_window, llm } => {
            let mut built: HashMap<String, Box<dyn trading_core::Strategy>> = HashMap::new();
            for (cname, cspec) in candidates {
                built.insert(
                    cname.clone(),
                    build_strategy(cspec, &format!("{name}-{cname}"), units, allow_pyramid, max_units, llm_key),
                );
            }
            let regimes = HashMap::from([
                ("trending".to_string(), trending.clone()),
                ("ranging".to_string(), ranging.clone()),
            ]);
            // Brain choice, logged: LLM only with spec + key (caller
            // forces None in live mode); otherwise the deterministic
            // rule brain. Either way the chain ends at tested candidates.
            let router = if let (Some(l), Some(key)) = (llm, llm_key) {
                println!("{name}: router brain = llm (rule fallback armed)");
                RouterStrategy::new(
                    name,
                    Box::new(LlmRouter::new(format!("{name}-router-llm"), key.to_string(), l.eval_every_n_ticks)),
                    built,
                    regimes,
                    default.clone(),
                    *trend_window,
                )
            } else {
                if llm.is_some() {
                    println!("{name}: router brain = rules (no LLM key)");
                }
                RouterStrategy::new(
                    name,
                    Box::new(RuleRouter::new(*trend_window)),
                    built,
                    regimes,
                    default.clone(),
                    *trend_window,
                )
            };
            // Every LLM-touching strategy goes through the hybrid
            // algorithmic fallback. Rebuild the default candidate for it.
            let fallback_spec = candidates
                .get(default)
                .expect("router default validated before build");
            let fallback = build_strategy(
                fallback_spec,
                &format!("{name}-fallback"),
                units,
                allow_pyramid,
                max_units,
                llm_key,
            );
            Box::new(HybridStrategy::new(name, Box::new(router), fallback))
        }
        StrategySpec::Llm { persona, fallback_fast, fallback_slow } => {
            let api_key = llm_key
                .expect("LLM agent without ANTHROPIC_API_KEY (caller must skip first)")
                .to_string();
            let llm = LlmStrategy::new(format!("{name}-llm"), api_key, units, persona.clone());
            let fallback = SmaCrossover::new(format!("{name}-fallback"), *fallback_fast, *fallback_slow, units)
                .with_inventory(allow_pyramid, max_units);
            Box::new(HybridStrategy::new(name, Box::new(llm), Box::new(fallback)))
        }
    }
}

fn print_event(agent_id: &str, tick: u32, event: &AgentEvent) {
    match event {
        AgentEvent::Tick { .. } => {}
        AgentEvent::OrderPlaced { order, price } => println!("[t{tick}] {agent_id} placed order: {order:?} @ {price:.5}"),        AgentEvent::OrderRejected(reason) => println!("[t{tick}] {agent_id} order rejected: {reason}"),
        AgentEvent::StopLossHit { price, move_pct, closed_units } => {
            println!("[t{tick}] 🛑 {agent_id} STOP-LOSS at {price:.5} ({:.2}%, {closed_units} units)", move_pct * 100.0)
        }
        AgentEvent::TakeProfitHit { price, move_pct, closed_units } => {
            println!("[t{tick}] ✅ {agent_id} TAKE-PROFIT at {price:.5} ({:.2}%, {closed_units} units)", move_pct * 100.0)
        }
        AgentEvent::Split { withdrawn, new_baseline } => {
            println!("[t{tick}] 🎉 {agent_id} DOUBLED — withdrawing ${withdrawn:.2}, continuing with ${new_baseline:.2}")
        }
        AgentEvent::RegimeSelected { strategy } => {
            println!("[t{tick}] 🧭 {agent_id} regime → {strategy}")
        }
        AgentEvent::Died { final_balance } => println!("[t{tick}] 💀 {agent_id} DIED — final balance ${final_balance:.2}"),
    }
}

fn persist_event(log: &EventLog, run_id: &str, agent_id: &str, tick: u32, event: &AgentEvent) {
    let (kind, data) = match event {
        AgentEvent::Tick { equity } => ("tick", serde_json::json!({ "equity": equity })),
        AgentEvent::OrderPlaced { order, price } => (
            "order_placed",
            serde_json::json!({ "symbol": order.symbol, "side": format!("{:?}", order.side), "units": order.units, "price": price }),
        ),
        AgentEvent::OrderRejected(reason) => ("order_rejected", serde_json::json!({ "reason": reason })),
        AgentEvent::StopLossHit { price, move_pct, closed_units } => {
            ("stop_loss_hit", serde_json::json!({ "price": price, "move_pct": move_pct, "closed_units": closed_units }))
        }
        AgentEvent::TakeProfitHit { price, move_pct, closed_units } => {
            ("take_profit_hit", serde_json::json!({ "price": price, "move_pct": move_pct, "closed_units": closed_units }))
        }
        AgentEvent::Split { withdrawn, new_baseline } => {
            ("split", serde_json::json!({ "withdrawn": withdrawn, "new_baseline": new_baseline }))
        }
        AgentEvent::RegimeSelected { strategy } => {
            ("regime_selected", serde_json::json!({ "strategy": strategy }))
        }
        AgentEvent::Died { final_balance } => ("died", serde_json::json!({ "final_balance": final_balance })),
    };
    let record = EventRecord {
        ts: Utc::now(),
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        tick,
        kind: kind.to_string(),
        data,
    };
    if let Err(e) = log.append(&record) {
        eprintln!("failed to persist event: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trading_core::Strategy as _;

    #[test]
    fn builds_nested_registry_without_llm_key() {
        let spec = StrategySpec::Atr {
            inner: Box::new(StrategySpec::NewsGated {
                inner: Box::new(StrategySpec::Rsi {
                    period: 14,
                    overbought: 70.0,
                    oversold: 30.0,
                }),
                cooldown_ticks: 5,
                sentiment_threshold: 0.5,
            }),
            period: 14,
            risk_pct: 0.02,
            max_units: 500.0,
        };
        let s = build_strategy(&spec, "test", 100.0, false, 100.0, None);
        assert!(s.name().contains("test"));
        assert!(s.is_healthy());
        assert!(!contains_llm(&spec));
    }

    #[test]
    fn contains_llm_finds_nesting_at_any_depth() {
        let plain = StrategySpec::Sma { fast: 5, slow: 20 };
        assert!(!contains_llm(&plain));
        let nested = StrategySpec::Atr {
            inner: Box::new(StrategySpec::NewsGated {
                inner: Box::new(StrategySpec::Llm {
                    persona: "x".to_string(),
                    fallback_fast: 5,
                    fallback_slow: 20,
                }),
                cooldown_ticks: 5,
                sentiment_threshold: 0.5,
            }),
            period: 14,
            risk_pct: 0.02,
            max_units: 500.0,
        };
        assert!(contains_llm(&nested));
        // Calendar gate nests too — an LLM under it must still be found.
        let cal = StrategySpec::CalendarGated {
            inner: Box::new(StrategySpec::Sma { fast: 5, slow: 20 }),
            window_minutes: 30,
        };
        assert!(!contains_llm(&cal));
    }

    #[test]
    fn builds_calendar_gated_stack() {
        let spec = StrategySpec::CalendarGated {
            inner: Box::new(StrategySpec::Donchian { channel: 20 }),
            window_minutes: 30,
        };
        let s = build_strategy(&spec, "cal", 100.0, false, 100.0, None);
        assert!(s.name().contains("cal"));
        assert!(s.is_healthy());
    }

    fn router_spec(with_llm: bool) -> StrategySpec {
        StrategySpec::Router {
            candidates: HashMap::from([
                ("mr".to_string(), Box::new(StrategySpec::Rsi {
                    period: 14, overbought: 70.0, oversold: 30.0,
                })),
                ("tr".to_string(), Box::new(StrategySpec::Donchian { channel: 20 })),
            ]),
            default: "mr".to_string(),
            trending: "tr".to_string(),
            ranging: "mr".to_string(),
            trend_window: 20,
            llm: with_llm.then(|| trading_config::RouterLlmSpec { eval_every_n_ticks: 10 }),
        }
    }

    #[test]
    fn builds_rule_router_without_key() {
        // No llm section, no key: pure-code brain, Hybrid-wrapped.
        let s = build_strategy(&router_spec(false), "rr", 100.0, false, 100.0, None);
        assert!(s.name().contains("rr"));
        assert!(s.is_healthy());
    }

    #[test]
    fn builds_llm_router_without_network() {
        // Construction performs no I/O; the key is only used per-tick.
        let spec = router_spec(true);
        assert!(!spec.requires_llm_key()); // degrades: never required
        assert!(contains_llm(&spec)); // ...but still flagged for live gating
        let s = build_strategy(&spec, "rl", 100.0, false, 100.0, Some("k"));
        assert!(s.name().contains("rl"));
    }

    #[test]
    fn symbols_of_groups_distinct_symbols() {
        let mk = |id: &str, symbol: &str| AgentSpec {
            id: id.to_string(),
            symbol: symbol.to_string(),
            stake_min: 10.0,
            stake_max: 100.0,
            units: 100.0,
            stop_loss_pct: None,
            take_profit_pct: None,
            allow_pyramid: false,
            max_position_units: None,
            min_units: None,
            min_notional: None,
            commission_per_unit: 0.0,
            spread_pips: None,
            strategy: StrategySpec::Sma { fast: 5, slow: 20 },
        };
        let agents = vec![mk("a", "EUR_USD"), mk("b", "EUR_USD"), mk("c", "GBP_USD")];
        assert_eq!(symbols_of(&agents), vec!["EUR_USD", "GBP_USD"]);
        assert!(symbols_of(&[]).is_empty());
    }
}
