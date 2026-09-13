use agent_runtime::{Agent, AgentEvent, AgentStatus};
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
use strategy_llm::LlmStrategy;
use strategy_sma::SmaCrossover;
use tokio::sync::{broadcast, mpsc};
use trading_config::{AgentSpec, Mode, RunConfig, StrategySpec};
use trading_core::{MarketFeed, NewsFeed, NewsItem};

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
    Final {
        agent_id: String,
        status: AgentStatus,
        equity: f64,
        withdrawn: f64,
        balance: f64,
        open_units: f64,
        entry_price: Option<f64>,
        baseline: f64,
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
    if matches!(config.mode, Mode::Live) {
        eprintln!("LIVE mode requested, but no live broker is compiled in (PaperBroker only). Refusing to run so paper fills are never mistaken for real execution. Use mode = \"test\" until broker-oanda + reconcile + margin checks land (see docs/PLAN.md:P0).");
        std::process::exit(1);
    }
    let is_live = matches!(config.mode, Mode::Live);

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
        let is_llm = matches!(spec.strategy, StrategySpec::Llm { .. });
        if is_llm && is_live {
            println!("Skipping '{}' — LLM-driven agents are not permitted in live mode.", spec.id);
            continue;
        }
        if is_llm && std::env::var("ANTHROPIC_API_KEY").is_err() {
            println!("Skipping '{}' — ANTHROPIC_API_KEY not set.", spec.id);
            continue;
        }

        let stake: f64 = if spec.stake_max > spec.stake_min {
            rng.gen_range(spec.stake_min..spec.stake_max)
        } else {
            spec.stake_min // fixed stake (min == max) must not panic gen_range
        };
        let strategy: Box<dyn trading_core::Strategy> = match &spec.strategy {
            StrategySpec::Sma { fast, slow } => {
                Box::new(SmaCrossover::new(spec.id.clone(), *fast, *slow, spec.units))
            }
            StrategySpec::Llm { persona, fallback_fast, fallback_slow } => {
                let api_key = std::env::var("ANTHROPIC_API_KEY").unwrap();
                let llm = LlmStrategy::new(format!("{}-llm", spec.id), api_key, spec.units, persona.clone());
                let fallback = SmaCrossover::new(format!("{}-fallback", spec.id), *fallback_fast, *fallback_slow, spec.units);
                Box::new(HybridStrategy::new(spec.id.clone(), Box::new(llm), Box::new(fallback)))
            }
        };

        let mut agent = match snapshots.get(&spec.id) {
            Some(snap) => {
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
                            Box::new(broker),
                            snap.baseline,
                            snap.withdrawn,
                        )
                    }
                    Err(e) => {
                        eprintln!(
                            "Snapshot for '{}' corrupt ({e}) — starting fresh with ${stake:.2}.",
                            spec.id
                        );
                        Agent::new(spec.id.clone(), spec.symbol.clone(), strategy, Box::new(PaperBroker::new(stake)), stake)
                    }
                }
            }
            None => Agent::new(spec.id.clone(), spec.symbol.clone(), strategy, Box::new(PaperBroker::new(stake)), stake),
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

    // --- Wire up broadcast (parallel tasks) + logging pipeline ---
    let (tick_tx, _) = broadcast::channel::<TickMsg>(1024);
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogMsg>();
    let alive_count = Arc::new(AtomicUsize::new(agents.len()));

    let mut handles = Vec::new();
    let snapshot_every = config.snapshot_every_n_ticks;
    for agent in agents {
        let mut rx = tick_tx.subscribe();
        let log_tx = log_tx.clone();
        let alive_count = alive_count.clone();
        let handle = tokio::spawn(async move {
            let mut agent = agent;
            let agent_id = agent.id.clone();
            loop {
                match rx.recv().await {
                    Ok(msg) => {
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
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
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
                    let record = EventRecord {
                        ts: Utc::now(),
                        run_id: logger_run_id.clone(),
                        agent_id: agent_id.clone(),
                        tick,
                        kind: "snapshot".to_string(),
                        data: serde_json::json!({
                            "balance": balance, "open_units": open_units,
                            "entry_price": entry_price, "baseline": baseline,
                            "withdrawn": withdrawn,
                        }),
                    };
                    let _ = logger_event_log.append(&record);
                }
                LogMsg::Final { agent_id, status, equity, withdrawn, balance, open_units, entry_price, baseline } => {
                    let record = EventRecord {
                        ts: Utc::now(),
                        run_id: logger_run_id.clone(),
                        agent_id: agent_id.clone(),
                        tick: 0,
                        kind: "final_summary".to_string(),
                        data: serde_json::json!({
                            "status": format!("{status:?}"), "equity": equity,
                            "withdrawn": withdrawn, "balance": balance,
                            "open_units": open_units, "entry_price": entry_price,
                            "baseline": baseline,
                        }),
                    };
                    let _ = logger_event_log.append(&record);
                    finals.push((agent_id, status, equity, withdrawn));
                }
            }
        }
        finals
    });

    // --- Producer: mock feed + news, broadcast to all agent tasks ---
    let mut feed = MockFeed::new(1.1000, 0.0006);
    let mut news_feed = MockNewsFeed::new(config.news_every_n_ticks);

    for tick in 0..config.ticks {
        if alive_count.load(Ordering::SeqCst) == 0 {
            println!("\nAll agents dead. Stopping early at tick {tick}.");
            break;
        }
        let candle = match feed.next_price(&config.agents[0].symbol).await {
            Some(c) => c,
            None => break,
        };
        let news = news_feed.next_headline().await;
        let _ = tick_tx.send(TickMsg { tick, candle, news });
        tokio::time::sleep(Duration::from_millis(config.tick_delay_ms)).await;
    }
    drop(tick_tx); // closes the channel; agent tasks exit their loops

    for handle in handles {
        let _ = handle.await;
    }
    let finals = logger.await.unwrap_or_default();

    println!("\n=== Final report ===");
    for (id, status, equity, withdrawn) in finals {
        println!("{id:<16} status={status:?} equity=${equity:>8.2} withdrawn_total=${withdrawn:.2}");
    }
    println!("\nFull event log: {log_path}");
}

fn print_event(agent_id: &str, tick: u32, event: &AgentEvent) {
    match event {
        AgentEvent::Tick { .. } => {}
        AgentEvent::OrderPlaced(order) => println!("[t{tick}] {agent_id} placed order: {order:?}"),
        AgentEvent::OrderRejected(reason) => println!("[t{tick}] {agent_id} order rejected: {reason}"),
        AgentEvent::StopLossHit { price, move_pct } => {
            println!("[t{tick}] 🛑 {agent_id} STOP-LOSS at {price:.5} ({:.2}%)", move_pct * 100.0)
        }
        AgentEvent::TakeProfitHit { price, move_pct } => {
            println!("[t{tick}] ✅ {agent_id} TAKE-PROFIT at {price:.5} ({:.2}%)", move_pct * 100.0)
        }
        AgentEvent::Split { withdrawn, new_baseline } => {
            println!("[t{tick}] 🎉 {agent_id} DOUBLED — withdrawing ${withdrawn:.2}, continuing with ${new_baseline:.2}")
        }
        AgentEvent::Died { final_balance } => println!("[t{tick}] 💀 {agent_id} DIED — final balance ${final_balance:.2}"),
    }
}

fn persist_event(log: &EventLog, run_id: &str, agent_id: &str, tick: u32, event: &AgentEvent) {
    let (kind, data) = match event {
        AgentEvent::Tick { equity } => ("tick", serde_json::json!({ "equity": equity })),
        AgentEvent::OrderPlaced(order) => (
            "order_placed",
            serde_json::json!({ "symbol": order.symbol, "side": format!("{:?}", order.side), "units": order.units }),
        ),
        AgentEvent::OrderRejected(reason) => ("order_rejected", serde_json::json!({ "reason": reason })),
        AgentEvent::StopLossHit { price, move_pct } => {
            ("stop_loss_hit", serde_json::json!({ "price": price, "move_pct": move_pct }))
        }
        AgentEvent::TakeProfitHit { price, move_pct } => {
            ("take_profit_hit", serde_json::json!({ "price": price, "move_pct": move_pct }))
        }
        AgentEvent::Split { withdrawn, new_baseline } => {
            ("split", serde_json::json!({ "withdrawn": withdrawn, "new_baseline": new_baseline }))
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
