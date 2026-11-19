//! Follows the transcript files coding agents already write, remembering how
//! far it read in each one. On the very first run it starts at the end of
//! existing files: without input data from before Pulse was installed, old
//! history could not be credited anyway.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapters::{Resolve, claude_code, codex};
use crate::store::Store;

#[derive(Default, Serialize, Deserialize)]
struct State {
    initialized: bool,
    files: HashMap<PathBuf, FileState>,
}

#[derive(Serialize, Deserialize)]
struct FileState {
    offset: u64,
    #[serde(default)]
    codex: Option<codex::Session>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Claude,
    Codex,
}

pub struct Tailer {
    state_path: PathBuf,
    state: State,
    /// `~/.claude/projects`, or `None` when Claude Code hooks are installed.
    pub claude_projects: Option<PathBuf>,
    pub codex_sessions: Option<PathBuf>,
}

impl Tailer {
    pub fn new(
        store: &Store,
        claude_projects: Option<PathBuf>,
        codex_sessions: Option<PathBuf>,
    ) -> Tailer {
        let state_path = store.root().join("state").join("transcripts.json");
        let state = fs::read(&state_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Tailer {
            state_path,
            state,
            claude_projects,
            codex_sessions,
        }
    }

    /// Reads whatever was appended since the last poll. Returns how many
    /// events were written to the store.
    pub fn poll(&mut self, store: &Store, resolve: Resolve) -> io::Result<usize> {
        let first_run = !self.state.initialized;
        let recent = SystemTime::now() - Duration::from_secs(172_800); // 2 days: 2 * 24 * 60 * 60
        let mut found = Vec::new();
        if let Some(dir) = &self.claude_projects {
            for project in read_dir(dir) {
                found.extend(
                    read_dir(&project)
                        .into_iter()
                        .filter(|p| has_extension(p))
                        .map(|p| (p, Format::Claude)),
                );
            }
        }
        if let Some(dir) = &self.codex_sessions {
            walk(dir, &mut |path| {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with("rollout-") && has_extension(path) {
                    found.push((path.to_path_buf(), Format::Codex));
                }
            });
        }

        let mut written = 0;
        let mut changed = first_run;
        let mut seen = Vec::new();
        for (path, format) in found {
            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.modified().is_ok_and(|modified| modified < recent) {
                continue;
            }
            seen.push(path.clone());
            let len = meta.len();
            let file_state = self
                .state
                .files
                .entry(path.clone())
                .or_insert_with(|| FileState {
                    offset: if first_run { len } else { 0 },
                    codex: None,
                });
            if format == Format::Codex && file_state.codex.is_none() {
                let mut session = codex::Session {
                    id: path
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned()),
                    ..Default::default()
                };
                // skipped history still holds the session's id and directory
                if file_state.offset > 0 {
                    for line in read_lines(&path, 0, file_state.offset)?.0 {
                        codex::read_line(&mut session, &line, resolve);
                    }
                }
                file_state.codex = Some(session);
            }
            if len < file_state.offset {
                file_state.offset = 0;
            }
            if len == file_state.offset {
                continue;
            }
            let (lines, consumed) = read_lines(&path, file_state.offset, len)?;
            file_state.offset += consumed;
            changed |= consumed > 0;
            for line in lines {
                let event = match format {
                    Format::Claude => claude_code::from_transcript_line(&line, resolve),
                    Format::Codex => codex::read_line(
                        file_state.codex.as_mut().expect("set above"),
                        &line,
                        resolve,
                    ),
                };
                if let Some(event) = event {
                    store.append_event(&event)?;
                    written += 1;
                }
            }
        }

        let before = self.state.files.len();
        self.state.files.retain(|path, _| seen.contains(path));
        if changed || before != self.state.files.len() {
            self.state.initialized = true;
            self.save()?;
        }
        Ok(written)
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = self.state_path.with_extension("json.tmp");
        fs::write(&temporary, serde_json::to_vec(&self.state)?)?;
        fs::rename(temporary, &self.state_path)
    }
}

fn has_extension(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
}

fn read_dir(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect()
        })
        .unwrap_or_default()
}

fn walk(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    for path in read_dir(dir) {
        if path.is_dir() {
            walk(&path, visit);
        } else {
            visit(&path);
        }
    }
}

/// Parses complete lines in `[from, to)`; a half-written last line is left
/// for the next poll. Returns the lines and how many bytes were consumed.
fn read_lines(path: &Path, from: u64, to: u64) -> io::Result<(Vec<Value>, u64)> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(from))?;
    let mut bytes = Vec::new();
    file.take(to - from).read_to_end(&mut bytes)?;
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |end| end + 1);
    let lines = bytes[..complete]
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice(line).ok())
        .collect();
    Ok((lines, complete as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse_core::EventKind;
    use std::io::Write;

    fn append(path: &Path, text: &str) {
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    #[test]
    fn skips_history_on_first_run_then_follows_new_lines_and_partial_writes() {
        let home = tempfile::tempdir().unwrap();
        let sessions = home.path().join("codex/2026/11/19");
        fs::create_dir_all(&sessions).unwrap();
        let file = sessions.join("rollout-2026-11-19T01-00-00-abc.jsonl");
        append(
            &file,
            "{\"timestamp\":\"2026-11-19T01:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"c1\",\"cwd\":\"/w/game\"}}\n",
        );
        append(
            &file,
            "{\"timestamp\":\"2026-11-19T01:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\"}}\n",
        );

        let store = Store::new(home.path().join("pulse"));
        let resolve = |path: &Path| path.display().to_string();
        let mut tailer = Tailer::new(&store, None, Some(home.path().join("codex")));
        assert_eq!(tailer.poll(&store, &resolve).unwrap(), 0);

        append(
            &file,
            "{\"timestamp\":\"2026-11-19T01:05:00Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\"}}\n{\"timestamp\":\"2026-11-19T01:05",
        );
        // a fresh tailer proves the offset and session context survive a restart
        let mut tailer = Tailer::new(&store, None, Some(home.path().join("codex")));
        assert_eq!(tailer.poll(&store, &resolve).unwrap(), 1);

        append(
            &file,
            ":30Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n",
        );
        assert_eq!(tailer.poll(&store, &resolve).unwrap(), 1);

        let events = store.events(1_795_000_000, 1_795_200_000);
        let summary: Vec<(EventKind, &str, &str)> = events
            .iter()
            .map(|e| (e.kind, e.session.as_str(), e.project.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (EventKind::PromptSubmitted, "c1", "/w/game"),
                (EventKind::TurnEnded, "c1", "/w/game")
            ]
        );
    }
}
