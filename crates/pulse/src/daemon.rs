//! The background loop. Every second it samples the OS idle timer and, while
//! a coding agent session is open, records one screen frame. Every few
//! seconds it follows agent transcripts and works out which sessions are open.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pulse_core::{Params, engaged_projects};

use crate::presence::IdleTimer;
use crate::recorder::Recorder;
use crate::store::Store;
use crate::tail::Tailer;
use crate::{now, paths, project, settings, sync};

/// How long recording keeps going after the last sign of you doing something
/// before it pauses: 120 seconds of no input and no human event stops it, and
/// the next input or action resumes it. The screen is recorded only while the
/// person is active; the agent working by itself does not keep it alive.
const RECORD_IDLE_SECONDS: i64 = 120;

/// The time of the most recent event the person made: a prompt, a terminal
/// command, a tool decision or a session start. Agent events (the agent
/// replying, doing something or finishing) do not count, because the recording
/// follows the person, not the agent.
fn last_human_event(events: &[pulse_core::Event]) -> i64 {
    events
        .iter()
        .filter(|event| event.kind.is_human())
        .map(|event| event.t)
        .max()
        .unwrap_or(0)
}

/// Whether the recorder should run for this second: a project is engaged and
/// the person did something recently. "Something" is keyboard or mouse input,
/// or a human event (a command started, a prompt sent, a tool approved). A
/// terminal command counts through its event, so typing in the terminal
/// starts the recording even where the OS idle timer is unavailable.
pub fn should_record(engaged: bool, now: i64, last_input: i64, last_human: i64) -> bool {
    if !engaged {
        return false;
    }
    let since_activity = (now - last_input).min(now - last_human);
    since_activity <= RECORD_IDLE_SECONDS
}

pub fn run(store: &Store, record: bool) -> io::Result<()> {
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
    let mut recorder = if record {
        match Recorder::open(store.root().join("recordings"), &store.root().join("codec")) {
            Ok(recorder) => Some(recorder),
            Err(reason) => {
                eprintln!(
                    "pulse: not recording the screen: {reason}. Time is still tracked but cannot be verified."
                );
                None
            }
        }
    } else {
        None
    };
    // finish the open segment on Ctrl+C or SIGTERM, so it is playable and in the chain
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)).map_err(io::Error::other)?;
    let uploader = sync::spawn(store.root().to_path_buf(), Arc::clone(&stop));
    let mut tailer = Tailer::new(
        store,
        None,
        paths::codex_dir().map(|dir| dir.join("sessions")),
    );
    let params = Params::default();
    let mut last_input = 0;
    let mut last_human = 0;
    let mut active = BTreeSet::new();
    let mut recording = false;
    eprintln!("pulse: tracking, data in {}", store.root().display());

    for tick in 0u64.. {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let now = now();
        let mut input = false;
        if let Some(idle_seconds) = idle.idle_seconds() {
            let input_at = now - idle_seconds as i64;
            input = idle_seconds == 0;
            if input_at > last_input {
                store.append_input(input_at)?;
                last_input = input_at;
            }
        }

        if tick % 5 == 0 {
            // with hooks installed, Claude Code's transcripts would only repeat what the hooks report
            if tick % 60 == 0 {
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

            let events = store.events(now - 3600, now + 1);
            last_human = last_human_event(&events);
            let engaged = engaged_projects(&events, now, &params);
            if engaged != active {
                if engaged.is_empty() {
                    eprintln!("pulse: session closed");
                } else {
                    let list: Vec<&str> = engaged.iter().map(String::as_str).collect();
                    eprintln!("pulse: session open for {}", list.join(", "));
                }
                active = engaged;
            }
        }

        // the screen is only recorded while something is actually happening
        let want = should_record(!active.is_empty(), now, last_input, last_human);
        if want != recording {
            recording = want;
            if recording {
                eprintln!("pulse: activity detected, recording");
            } else {
                eprintln!("pulse: nothing happening, recording paused");
                if let Some(recorder) = &mut recorder {
                    recorder.finish()?;
                }
            }
        }
        if recording
            && let Some(recorder) = &mut recorder
            && let Err(error) = recorder.record(now, input, &active)
        {
            eprintln!("pulse: recording failed: {error}");
        }

        if stop.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(until_next_second());
    }
    if let Some(recorder) = &mut recorder {
        recorder.finish()?;
    }
    let _ = uploader.join();
    eprintln!("pulse: stopped");
    Ok(())
}

pub fn until_next_second() -> Duration {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_millis());
    Duration::from_millis(u64::from(1000 - millis))
}

fn claude_projects_without_hooks() -> Option<PathBuf> {
    let dir = paths::claude_dir()?;
    let hooks_installed = settings::read(&dir.join("settings.json"))
        .is_ok_and(|settings| crate::install::installed(&settings));
    (!hooks_installed).then(|| dir.join("projects"))
}

#[cfg(test)]
mod tests {
    use super::{last_human_event, should_record};
    use pulse_core::{Event, EventKind, PROTOCOL_VERSION};

    fn event(t: i64, kind: EventKind) -> Event {
        Event {
            v: PROTOCOL_VERSION,
            t,
            source: "claude-code".into(),
            session: "s".into(),
            project: "p".into(),
            kind,
            model: None,
        }
    }

    #[test]
    fn recording_needs_an_engaged_project_and_recent_human_activity() {
        let now = 1_795_200_000;
        // not engaged: never record
        assert!(!should_record(false, now, now, now));
        // engaged with fresh input, or a fresh human event with no input
        assert!(should_record(true, now, now, 0));
        assert!(should_record(true, now, now - 121, now));
        // exactly at the boundary still counts
        assert!(should_record(true, now, now - 120, 0));
        // both quiet for too long: pause
        assert!(!should_record(true, now, now - 121, now - 121));
        // no input and no human event ever seen
        assert!(!should_record(true, now, 0, 0));
    }

    #[test]
    fn only_person_events_count_as_activity_for_recording() {
        let events = [
            event(100, EventKind::AgentActivity),
            event(200, EventKind::TurnEnded),
            event(300, EventKind::PromptSubmitted),
            event(400, EventKind::AgentActivity),
        ];
        // the agent's later events are ignored; the last thing the person did was at 300
        assert_eq!(last_human_event(&events), 300);
        // agent work alone is no activity at all
        let agent_only = [
            event(100, EventKind::AgentActivity),
            event(200, EventKind::TurnEnded),
        ];
        assert_eq!(last_human_event(&agent_only), 0);
    }
}
