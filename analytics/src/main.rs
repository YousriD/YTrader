//! analytics CLI: report + track + gate over run logs.
//!
//! ```bash
//! cargo run -p analytics -- report data/run-20240101T000000.jsonl
//! cargo run -p analytics -- report data/run-xxx.jsonl --json
//! cargo run -p analytics -- gate data/run-xxx.jsonl [--min-trades 20 ...]
//! cargo run -p analytics -- track data/run-xxx.jsonl [interval_secs]
//! ```
//! `track` re-reads the file every interval and reprints the table —
//! point it at a live run's log to watch a session. Plain std (no TUI)
//! by decision: GUI waits until selling is real (see docs/PLAN.md).

use analytics::{analyze, evaluate, Thresholds};
use persistence::EventLog;

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  analytics report <log.jsonl> [--json]");
    eprintln!("  analytics gate <log.jsonl> [--json] [--min-trades N] [--min-profit-factor X]");
    eprintln!("      [--min-win-rate X] [--max-drawdown X] [--min-sharpe X] [--min-ticks N]");
    eprintln!("  analytics track <log.jsonl> [interval_secs]");
    std::process::exit(2);
}

fn get_flag(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

fn thresholds_from(args: &[String]) -> Thresholds {
    let mut t = Thresholds::default();
    let num = |n: &str| get_flag(args, n).and_then(|v| v.parse::<f64>().ok());
    let int = |n: &str| get_flag(args, n).and_then(|v| v.parse::<usize>().ok());
    let int32 = |n: &str| get_flag(args, n).and_then(|v| v.parse::<u32>().ok());
    if let Some(v) = int("--min-trades") {
        t.min_trades = v;
    }
    if let Some(v) = num("--min-profit-factor") {
        t.min_profit_factor = v;
    }
    if let Some(v) = num("--min-win-rate") {
        t.min_win_rate_pct = v;
    }
    if let Some(v) = num("--max-drawdown") {
        t.max_drawdown_pct = v;
    }
    if let Some(v) = num("--min-sharpe") {
        t.min_sharpe_per_tick = v;
    }
    if let Some(v) = int32("--min-ticks") {
        t.min_ticks = v;
    }
    t
}

fn load(path: &str) -> Vec<persistence::EventRecord> {
    match EventLog::read_all(path) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            std::process::exit(1);
        }
    }
}

fn print_text(report: &analytics::Report) {
    println!("run: {}", report.run_id.as_deref().unwrap_or("(unknown)"));
    println!(
        "{:<16} {:>6} {:>7} {:>6} {:>6} {:>8} {:>8} {:>9} {:>7} {:>5} {:>4}",
        "agent", "ticks", "trades", "win%", "pf", "avgwin", "avgloss", "maxdd%", "ret%", "banked", "dead"
    );
    for a in &report.agents {
        println!(
            "{:<16} {:>6} {:>6} {:>6.1} {:>6} {:>8.2} {:>8.2} {:>9.1} {:>7.1} {:>5.0} {:>4}",
            a.agent_id,
            a.ticks,
            a.trade_stats.trades,
            a.trade_stats.win_rate_pct,
            a.trade_stats.profit_factor.map(|p| format!("{p:.2}")).unwrap_or_else(|| "n/a".to_string()),
            a.trade_stats.avg_win,
            a.trade_stats.avg_loss,
            a.max_drawdown_pct,
            a.total_return_pct,
            a.total_withdrawn,
            if a.died { "yes" } else { "" },
        );
    }
    let p = &report.portfolio;
    println!(
        "portfolio: agents={} return={:.1}% maxdd={:.1}% sharpe={:.3} banked=${:.0} deaths={}",
        p.agents, p.total_return_pct, p.max_drawdown_pct, p.sharpe_per_tick, p.total_withdrawn, p.deaths,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
    }
    match args[0].as_str() {
        "report" => {
            let path = args.get(1).unwrap_or_else(|| usage());
            let report = analyze(&load(path));
            if args.iter().any(|a| a == "--json") {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print_text(&report);
            }
        }
        "gate" => {
            let path = args.get(1).unwrap_or_else(|| usage());
            let report = analyze(&load(path));
            let t = thresholds_from(&args);
            let mut all_pass = true;
            for a in &report.agents {
                let g = evaluate(a, &t);
                if g.pass {
                    println!("PASS {}", g.agent_id);
                } else {
                    all_pass = false;
                    println!("FAIL {}:", g.agent_id);
                    for f in &g.failures {
                        println!("  - {f}");
                    }
                }
            }
            if args.iter().any(|a| a == "--json") {
                let gates: Vec<_> = report.agents.iter().map(|a| evaluate(a, &t)).collect();
                println!("{}", serde_json::to_string_pretty(&gates).unwrap());
            }
            if !all_pass {
                std::process::exit(1);
            }
        }
        "track" => {
            let path = args.get(1).unwrap_or_else(|| usage()).clone();
            let interval: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2).max(1);
            loop {
                print!("\x1B[2J\x1B[1;1H"); // clear screen
                print_text(&analyze(&load(&path)));
                println!("\n(tracking {path} every {interval}s — Ctrl+C to stop)");
                std::thread::sleep(std::time::Duration::from_secs(interval));
            }
        }
        _ => usage(),
    }
}
