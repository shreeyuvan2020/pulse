//! OpenCode: a small plugin in OpenCode's plugin folder reports prompts and
//! agent work by running `pulse hook opencode`. OpenCode keeps its sessions in
//! a database whose layout is not stable, so Pulse does not read it.

use std::path::Path;

use pulse_core::{Event, EventKind, PROTOCOL_VERSION};
use serde_json::Value;

use super::{Resolve, text};

pub const SOURCE: &str = "opencode";

/// The plugin file's name inside OpenCode's `plugins` folder.
pub const PLUGIN_FILE: &str = "pulse.js";

/// First line of every plugin Pulse writes, so it never touches a file it did not write.
pub const PLUGIN_MARKER: &str = "// Installed by Pulse";

const PLUGIN: &str = include_str!("opencode-plugin.js");

/// The plugin with the path to this `pulse` program filled in.
pub fn plugin_source(exe: &str) -> String {
    let quoted = serde_json::to_string(exe).expect("a string is always JSON");
    PLUGIN.replace("__PULSE_EXE__", &quoted)
}

/// What the plugin sends: `{"kind", "session", "cwd", "model"}`, ids and names only.
pub fn from_hook(input: &Value, now: i64, resolve: Resolve) -> Option<Event> {
    let kind = match text(input, "kind")? {
        "session_started" => EventKind::SessionStarted,
        "prompt_submitted" => EventKind::PromptSubmitted,
        "agent_activity" => EventKind::AgentActivity,
        "turn_ended" => EventKind::TurnEnded,
        "session_ended" => EventKind::SessionEnded,
        _ => return None,
    };
    Some(Event {
        v: PROTOCOL_VERSION,
        t: now,
        source: SOURCE.into(),
        session: text(input, "session")?.into(),
        project: resolve(Path::new(text(input, "cwd")?)),
        kind,
        model: text(input, "model")
            .filter(|_| kind != EventKind::SessionStarted)
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resolve(path: &Path) -> String {
        format!("resolved:{}", path.display())
    }

    #[test]
    fn plugin_events_become_pulse_events() {
        let event = |input: Value| from_hook(&input, 100, &resolve);
        let prompt = event(json!({"kind": "prompt_submitted", "session": "ses_1", "cwd": "/home/rowan/cs2", "model": "claude-opus-4-5"})).unwrap();
        assert_eq!(prompt.source, "opencode");
        assert_eq!(prompt.kind, EventKind::PromptSubmitted);
        assert_eq!(prompt.project, "resolved:/home/rowan/cs2");
        assert_eq!(prompt.model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(prompt.t, 100);

        let started = event(
            json!({"kind": "session_started", "session": "ses_1", "cwd": "/x", "model": "m"}),
        )
        .unwrap();
        assert_eq!(started.kind, EventKind::SessionStarted);
        assert_eq!(started.model, None);

        for (kind, expected) in [
            ("agent_activity", EventKind::AgentActivity),
            ("turn_ended", EventKind::TurnEnded),
            ("session_ended", EventKind::SessionEnded),
        ] {
            let found = event(json!({"kind": kind, "session": "s", "cwd": "/x"})).unwrap();
            assert_eq!(found.kind, expected);
            assert_eq!(found.model, None);
        }
    }

    #[test]
    fn unknown_or_incomplete_payloads_are_ignored() {
        let event = |input: Value| from_hook(&input, 1, &resolve);
        assert!(event(json!({"kind": "something_new", "session": "s", "cwd": "/x"})).is_none());
        assert!(event(json!({"kind": "turn_ended", "cwd": "/x"})).is_none());
        assert!(event(json!({"kind": "turn_ended", "session": "s"})).is_none());
        assert!(event(json!({"session": "s", "cwd": "/x"})).is_none());
    }

    #[test]
    fn the_plugin_gets_a_safely_quoted_path_and_the_marker() {
        let source = plugin_source(r#"C:\Users\Ro "wan"\pulse.exe"#);
        assert!(source.starts_with(PLUGIN_MARKER));
        assert!(source.contains(r#"const PULSE = "C:\\Users\\Ro \"wan\"\\pulse.exe";"#));
        assert!(!source.contains("__PULSE_EXE__"));
    }
}
