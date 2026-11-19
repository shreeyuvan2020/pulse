//! The local spool: plain JSON Lines files, one per UTC day, so people can
//! read exactly what Pulse recorded. Hooks append to it from short-lived
//! processes, so every record is a single small append.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate};
use pulse_core::Event;

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Store {
        Store { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn append_event(&self, event: &Event) -> io::Result<()> {
        let mut line = serde_json::to_string(event)?;
        line.push('\n');
        append(&self.day_file("events", event.t), &line)
    }

    /// Records that the OS saw keyboard or mouse input at `t`.
    pub fn append_input(&self, t: i64) -> io::Result<()> {
        append(&self.day_file("inputs", t), &format!("{t}\n"))
    }

    pub fn events(&self, from: i64, to: i64) -> Vec<Event> {
        self.read_lines("events", from, to)
            .filter_map(|line| serde_json::from_str::<Event>(&line).ok())
            .filter(|event| event.t >= from && event.t < to)
            .collect()
    }

    pub fn inputs(&self, from: i64, to: i64) -> Vec<i64> {
        self.read_lines("inputs", from, to)
            .filter_map(|line| line.trim().parse::<i64>().ok())
            .filter(|t| *t >= from && *t < to)
            .collect()
    }

    pub fn day_file(&self, kind: &str, t: i64) -> PathBuf {
        self.root.join(kind).join(format!("{}.jsonl", utc_date(t)))
    }

    fn read_lines(&self, kind: &str, from: i64, to: i64) -> impl Iterator<Item = String> {
        let mut files = Vec::new();
        let mut day = utc_date(from);
        let last = utc_date(to);
        while day <= last {
            files.push(self.root.join(kind).join(format!("{day}.jsonl")));
            day = day.succ_opt().expect("date in range");
        }
        files
            .into_iter()
            .filter_map(|path| File::open(path).ok())
            .flat_map(|file| BufReader::new(file).lines().map_while(Result::ok))
    }
}

fn utc_date(t: i64) -> NaiveDate {
    DateTime::from_timestamp(t, 0)
        .expect("timestamp in range")
        .date_naive()
}

fn append(path: &Path, line: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse_core::{EventKind, PROTOCOL_VERSION};

    #[test]
    fn reads_back_across_utc_midnight() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_path_buf());
        let midnight = 1_763_510_400; // 2025-11-19T00:00:00Z
        for t in [midnight - 10, midnight + 10, midnight + 90_000] {
            store
                .append_event(&Event {
                    v: PROTOCOL_VERSION,
                    t,
                    source: "codex".into(),
                    session: "s".into(),
                    project: "p".into(),
                    kind: EventKind::AgentActivity,
                    model: None,
                })
                .unwrap();
            store.append_input(t).unwrap();
        }

        let times: Vec<i64> = store
            .events(midnight - 60, midnight + 60)
            .iter()
            .map(|e| e.t)
            .collect();
        assert_eq!(times, vec![midnight - 10, midnight + 10]);
        assert_eq!(
            store.inputs(midnight - 60, midnight + 60),
            vec![midnight - 10, midnight + 10]
        );
        assert!(
            store
                .day_file("events", midnight - 10)
                .ends_with("2025-11-18.jsonl")
        );
    }
}
