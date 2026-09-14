//! Durable, append-only event log.
//!
//! Every meaningful thing that happens (order placed, stop-loss hit,
//! split, death) gets appended here as one JSON line. This is
//! deliberately the *first* persistence primitive, not the last —
//! it's dependency-light (no server, no schema migrations) so a
//! single-user tool can ship it as-is, but the record shape is meant
//! to be replayed into a real database later without changing what
//! callers write.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    pub ts: DateTime<Utc>,
    pub run_id: String,
    pub agent_id: String,
    pub tick: u32,
    /// e.g. "order_placed", "stop_loss_hit", "split", "died", "final_summary"
    pub kind: String,
    /// Arbitrary structured payload for this event kind.
    pub data: serde_json::Value,
}

pub struct EventLog {
    file: Mutex<File>,
    path: PathBuf,
}

impl EventLog {
    /// Opens (creating if needed) an append-only log file.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self { file: Mutex::new(file), path })
    }

    /// Appends one event. Each call is a single `write` of one JSON
    /// line, so a crash mid-write can corrupt at most the last record.
    pub fn append(&self, record: &EventRecord) -> io::Result<()> {
        let line = serde_json::to_string(record).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut file = self.file.lock().unwrap();
        writeln!(file, "{line}")?;
        file.flush()
    }

    /// Replays every record in the log, in order. Use this on startup
    /// to reconstruct state, or offline for an audit trail / P&L report.
    pub fn read_all(path: impl AsRef<Path>) -> io::Result<Vec<EventRecord>> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut out = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<EventRecord>(&line) {
                Ok(record) => out.push(record),
                Err(e) => eprintln!("skipping corrupt event log line: {e}"),
            }
        }
        Ok(out)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Restorable per-agent state for `--resume` (P1-1): broker cash and
/// position plus the split-baseline and lifetime withdrawals.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub balance: f64,
    pub open_units: f64,
    pub entry_price: Option<f64>,
    pub baseline: f64,
    pub withdrawn: f64,
}

/// Newest `run-*.jsonl` in `dir` (filenames are timestamped, so lexical
/// max == newest), or `None` if the directory has no runs yet.
pub fn latest_run_file(dir: impl AsRef<Path>) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir.as_ref()).ok()?;
    entries
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|n| n.starts_with("run-") && n.ends_with(".jsonl"))
        .max()
        .map(|n| dir.as_ref().join(n))
}

/// Build the `snapshot` record the orchestrator persists every
/// `snapshot_every_n_ticks`. logger + tests share this constructor so
/// the on-disk shape cannot drift from what `load_snapshots` reads.
pub fn snapshot_record(run_id: &str, agent_id: &str, tick: u32, snap: &Snapshot) -> EventRecord {
    EventRecord {
        ts: Utc::now(),
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        tick,
        kind: "snapshot".to_string(),
        data: serde_json::json!({
            "balance": snap.balance, "open_units": snap.open_units,
            "entry_price": snap.entry_price, "baseline": snap.baseline,
            "withdrawn": snap.withdrawn,
        }),
    }
}

/// Build the terminal `final_summary` record: same state keys as a
/// snapshot plus status/equity. `tick` is max so it sorts after every
/// tick record from the run.
pub fn final_record(run_id: &str, agent_id: &str, status: &str, equity: f64, snap: &Snapshot) -> EventRecord {
    EventRecord {
        ts: Utc::now(),
        run_id: run_id.to_string(),
        agent_id: agent_id.to_string(),
        tick: u32::MAX,
        kind: "final_summary".to_string(),
        data: serde_json::json!({
            "status": status, "equity": equity,
            "balance": snap.balance, "open_units": snap.open_units,
            "entry_price": snap.entry_price, "baseline": snap.baseline,
            "withdrawn": snap.withdrawn,
        }),
    }
}

fn json_num(data: &serde_json::Value, key: &str) -> Option<f64> {
    data.get(key)?.as_f64()
}

fn snapshot_from_data(data: &serde_json::Value) -> Option<Snapshot> {
    Some(Snapshot {
        balance: json_num(data, "balance")?,
        open_units: json_num(data, "open_units")?,
        entry_price: data.get("entry_price").and_then(|v| {
            if v.is_null() {
                Some(None)
            } else {
                v.as_f64().map(Some)
            }
        })?,
        baseline: json_num(data, "baseline")?,
        withdrawn: json_num(data, "withdrawn")?,
    })
}

/// Fold a log file in order down to the latest live snapshot per agent.
/// `snapshot` and alive `final_summary` records store state; `died` and
/// dead `final_summary` records remove the agent (a dead agent must
/// never resurrect — a fresh run starts a new one). All other kinds
/// (ticks, orders, SL/TP, splits) are ignored: snapshots already embody
/// their effects, which keeps replay from duplicating broker math.
pub fn load_snapshots(path: impl AsRef<Path>) -> io::Result<HashMap<String, Snapshot>> {
    let mut out: HashMap<String, Snapshot> = HashMap::new();
    for record in EventLog::read_all(path)? {
        match record.kind.as_str() {
            "snapshot" => {
                if let Some(snap) = snapshot_from_data(&record.data) {
                    out.insert(record.agent_id, snap);
                }
            }
            "final_summary" => {
                let dead = record.data.get("status").and_then(|s| s.as_str()) == Some("Dead");
                if dead {
                    out.remove(&record.agent_id);
                } else if let Some(snap) = snapshot_from_data(&record.data) {
                    out.insert(record.agent_id, snap);
                }
            }
            "died" => {
                out.remove(&record.agent_id);
            }
            _ => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(agent: &str, tick: u32, kind: &str, data: serde_json::Value) -> EventRecord {
        EventRecord {
            ts: Utc::now(),
            run_id: "test-run".to_string(),
            agent_id: agent.to_string(),
            tick,
            kind: kind.to_string(),
            data,
        }
    }

    fn snap_data(balance: f64, units: f64, entry: Option<f64>, baseline: f64, withdrawn: f64) -> serde_json::Value {
        serde_json::json!({
            "balance": balance, "open_units": units, "entry_price": entry,
            "baseline": baseline, "withdrawn": withdrawn,
        })
    }

    #[test]
    fn round_trip_and_corrupt_line_skipped() {
        let dir = std::env::temp_dir().join("tradery-persist-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roundtrip.jsonl");
        let _ = std::fs::remove_file(&path);
        let log = EventLog::open(&path).unwrap();
        log.append(&record("a", 1, "tick", serde_json::json!({"equity": 100.0}))).unwrap();
        {
            // Interleave one corrupt line: must be skipped, not fatal.
            use std::io::Write as _;
            let mut f = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(f, "this is not json").unwrap();
        }
        log.append(&record("a", 2, "snapshot", snap_data(90.0, 10.0, Some(1.0), 100.0, 0.0))).unwrap();
        let all = EventLog::read_all(&path).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].kind, "snapshot");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn fold_keeps_latest_and_never_resurrects_dead() {
        let dir = std::env::temp_dir().join("tradery-persist-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fold.jsonl");
        let _ = std::fs::remove_file(&path);
        let log = EventLog::open(&path).unwrap();
        log.append(&record("alive", 10, "snapshot", snap_data(80.0, 5.0, Some(1.2), 100.0, 20.0))).unwrap();
        log.append(&record("alive", 20, "snapshot", snap_data(85.0, 5.0, Some(1.2), 100.0, 20.0))).unwrap();
        log.append(&record("dead", 10, "snapshot", snap_data(50.0, 0.0, None, 100.0, 0.0))).unwrap();
        log.append(&record("dead", 11, "died", serde_json::json!({"final_balance": 0.0}))).unwrap();
        // A stale snapshot followed by a dead final must stay dead.
        log.append(&record("dead2", 10, "snapshot", snap_data(50.0, 0.0, None, 100.0, 0.0))).unwrap();
        log.append(&record(
            "dead2",
            0,
            "final_summary",
            serde_json::json!({"status": "Dead", "equity": 0.0, "withdrawn": 0.0,
                "balance": 0.0, "open_units": 0.0, "entry_price": null,
                "baseline": 100.0, "withdrawn": 0.0}),
        )).unwrap();
        let map = load_snapshots(&path).unwrap();
        assert_eq!(map.len(), 1);
        let a = &map["alive"];
        assert_eq!(a.balance, 85.0); // latest wins
        assert_eq!(a.entry_price, Some(1.2));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn latest_run_file_picks_newest() {
        let dir = std::env::temp_dir().join("tradery-latest-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(latest_run_file(&dir).is_none());
        File::create(dir.join("run-20260101T000000.jsonl")).unwrap();
        File::create(dir.join("run-20260601T000000.jsonl")).unwrap();
        File::create(dir.join("notes.txt")).unwrap();
        let latest = latest_run_file(&dir).unwrap();
        assert!(latest.ends_with("run-20260601T000000.jsonl"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
