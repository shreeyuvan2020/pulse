//! The background loop: every few seconds it samples the OS idle timer,
//! follows agent transcripts, and works out whether an agent session is open
//! (which is when the screen recorder will run).

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use pulse_core::{Params, engaged_projects};

use crate::presence::IdleTimer;
use crate::store::Store;
use crate::tail::Tailer;
use crate::{now, paths, project, settings};

pub fn run(store: &Store) -> io::Result<()> {
    std::fs::create_dir_all(store.root())?;
    let lock = File::create(store.root().join("daemon.lock"))?;
    if lock.try_lock().is_err() {
        return Err(io::Error::other("another Pulse daemon is already running"));
    }

    let mut idle = IdleTimer::new();
    if idle.idle_seconds().is_none() {
        eprintln!(
            "pulse: this system's idle timer is unavailable, so prompts are the only sign you are present"
        );
    }
    let mut tailer = Tailer::new(
        store,
        None,
        paths::codex_dir().map(|dir| dir.join("sessions")),
    );
    let params = Params::default();
    let mut last_input = 0;
    let mut active = BTreeSet::new();
    eprintln!("pulse: tracking, data in {}", store.root().display());

    for tick in 0u64.. {
        let now = now();
        if let Some(idle_seconds) = idle.idle_seconds() {
            let input_at = now - idle_seconds as i64;
            if input_at > last_input {
                store.append_input(input_at)?;
                last_input = input_at;
            }
        }

        // with hooks installed, Claude Code's transcripts would only repeat what the hooks report
        if tick % 12 == 0 {
            tailer.claude_projects = claude_projects_without_hooks();
        }
        let cache = RefCell::new(HashMap::<PathBuf, String>::new());
        let resolve = |cwd: &Path| {
            cache
                .borrow_mut()
                .entry(cwd.to_path_buf())
                .or_insert_with(|| project::resolve(cwd))
                .clone()
        };
        if let Err(error) = tailer.poll(store, &resolve) {
            eprintln!("pulse: reading transcripts failed: {error}");
        }

        let engaged = engaged_projects(&store.events(now - 3600, now + 1), now, &params);
        if engaged != active {
            if engaged.is_empty() {
                eprintln!("pulse: agent session closed, recording would stop");
            } else {
                let list: Vec<&str> = engaged.iter().map(String::as_str).collect();
                eprintln!(
                    "pulse: agent session open for {}, recording would run",
                    list.join(", ")
                );
            }
            active = engaged;
        }

        thread::sleep(Duration::from_secs(5));
    }
    Ok(())
}

fn claude_projects_without_hooks() -> Option<PathBuf> {
    let dir = paths::claude_dir()?;
    let hooks_installed = settings::read(&dir.join("settings.json"))
        .is_ok_and(|settings| crate::install::installed(&settings));
    (!hooks_installed).then(|| dir.join("projects"))
}
