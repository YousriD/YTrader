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
