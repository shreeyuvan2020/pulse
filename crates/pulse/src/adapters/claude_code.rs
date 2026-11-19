//! Claude Code: hooks (preferred, real time) and transcripts under
//! `~/.claude/projects/*/*.jsonl` (no setup, used when hooks are not installed).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use pulse_core::{Event, EventKind, PROTOCOL_VERSION};
use serde_json::Value;

use super::{Resolve, parse_time, text};

pub const SOURCE: &str = "claude-code";

/// Hook events Pulse registers in Claude Code's settings.
pub const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PostToolUse",
    "Stop",
    "SubagentStop",
    "SessionEnd",
];

pub fn from_hook(input: &Value, now: i64, resolve: Resolve) -> Option<Event> {
    let kind = match text(input, "hook_event_name")? {
        "SessionStart" => EventKind::SessionStarted,
        "UserPromptSubmit" => EventKind::PromptSubmitted,
        "PostToolUse" | "SubagentStop" => EventKind::AgentActivity,
        "Stop" => EventKind::TurnEnded,
        "SessionEnd" => EventKind::SessionEnded,
        _ => return None,
    };
    // hook payloads carry no model, so take it from the end of the transcript when a turn ends
    let model = (kind == EventKind::TurnEnded)
        .then(|| text(input, "transcript_path"))
        .flatten()
        .and_then(|path| last_model(Path::new(path)));
    Some(Event {
        v: PROTOCOL_VERSION,
        t: now,
        source: SOURCE.into(),
        session: text(input, "session_id")?.into(),
        project: resolve(Path::new(text(input, "cwd")?)),
        kind,
        model,
    })
}

pub fn from_transcript_line(line: &Value, resolve: Resolve) -> Option<Event> {
    // subagent work is the agent's, not the person's
    let sidechain = line.get("isSidechain").and_then(Value::as_bool) == Some(true);
    let meta = line.get("isMeta").and_then(Value::as_bool) == Some(true);
    let message = line.get("message");
    let (kind, model) = match text(line, "type")? {
        "user" if meta => return None,
        "user" if sidechain || is_tool_result(message?) => (EventKind::AgentActivity, None),
        "user" if is_command_output(message?) => return None,
        "user" => (EventKind::PromptSubmitted, None),
        "assistant" => (
            EventKind::AgentActivity,
            message
                .and_then(|m| text(m, "model"))
                .filter(|model| !model.starts_with('<'))
                .map(str::to_owned),
        ),
        _ => return None,
    };
    Some(Event {
        v: PROTOCOL_VERSION,
        t: parse_time(line.get("timestamp"))?,
        source: SOURCE.into(),
        session: text(line, "sessionId")?.into(),
        project: resolve(Path::new(text(line, "cwd")?)),
        kind,
        model,
    })
}

fn is_tool_result(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| text(item, "type") == Some("tool_result"))
        })
}

fn is_command_output(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_str)
        .is_some_and(|content| content.starts_with("<local-command-stdout>"))
}

fn last_model(transcript: &Path) -> Option<String> {
    let mut file = File::open(transcript).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(65536))).ok()?;
    let mut tail = String::new();
    file.take(65536).read_to_string(&mut tail).ok()?;
    tail.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|line| {
            from_transcript_line(&line, &|_| String::new()).and_then(|event| event.model)
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
    fn maps_hook_payloads_without_copying_prompt_text() {
        let input = json!({
            "session_id": "s1",
            "transcript_path": "/nonexistent.jsonl",
            "cwd": "/home/rowan/cs2-web",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "fix the recoil desync"
        });

        let event = from_hook(&input, 100, &resolve).unwrap();

        assert_eq!(event.kind, EventKind::PromptSubmitted);
        assert_eq!(event.project, "resolved:/home/rowan/cs2-web");
        assert_eq!(event.session, "s1");
        assert!(!serde_json::to_string(&event).unwrap().contains("recoil"));
        assert!(
            from_hook(
                &json!({"hook_event_name": "Notification", "session_id": "s1", "cwd": "/"}),
                1,
                &resolve
            )
            .is_none()
        );
    }

    #[test]
    fn stop_hook_reads_model_from_the_transcript() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("s1.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"type":"assistant","timestamp":"2026-11-19T01:00:00Z","sessionId":"s1","cwd":"/p","message":{"model":"claude-sonnet-4-5","content":[]}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-11-19T01:00:05Z","sessionId":"s1","cwd":"/p","message":{"model":"claude-opus-4-5","content":[]}}"#,
                "\n"
            ),
        )
        .unwrap();
        let input = json!({"session_id": "s1", "cwd": "/p", "hook_event_name": "Stop", "transcript_path": transcript});

        let event = from_hook(&input, 100, &resolve).unwrap();

        assert_eq!(event.kind, EventKind::TurnEnded);
        assert_eq!(event.model.as_deref(), Some("claude-opus-4-5"));
    }

    #[test]
    fn classifies_transcript_lines() {
        let base = |extra: Value| {
            let mut line =
                json!({"timestamp": "2026-11-19T01:00:00.123Z", "sessionId": "s1", "cwd": "/p"});
            line.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            from_transcript_line(&line, &resolve).map(|event| (event.kind, event.model))
        };

        assert_eq!(
            base(
                json!({"type": "user", "message": {"role": "user", "content": "add a scoreboard"}})
            ),
            Some((EventKind::PromptSubmitted, None))
        );
        assert_eq!(
            base(
                json!({"type": "user", "message": {"content": [{"type": "text", "text": "and sounds"}]}})
            ),
            Some((EventKind::PromptSubmitted, None))
        );
        assert_eq!(
            base(
                json!({"type": "user", "message": {"content": [{"type": "tool_result", "content": "ok"}]}})
            ),
            Some((EventKind::AgentActivity, None))
        );
        assert_eq!(
            base(
                json!({"type": "user", "isSidechain": true, "message": {"content": "subagent task"}})
            ),
            Some((EventKind::AgentActivity, None))
        );
        assert_eq!(
            base(json!({"type": "user", "isMeta": true, "message": {"content": "Caveat"}})),
            None
        );
        assert_eq!(
            base(
                json!({"type": "user", "message": {"content": "<local-command-stdout>done</local-command-stdout>"}})
            ),
            None
        );
        assert_eq!(
            base(
                json!({"type": "assistant", "message": {"model": "claude-opus-4-5", "content": []}})
            ),
            Some((EventKind::AgentActivity, Some("claude-opus-4-5".into())))
        );
        assert_eq!(
            base(json!({"type": "assistant", "message": {"model": "<synthetic>", "content": []}})),
            Some((EventKind::AgentActivity, None))
        );
        assert_eq!(base(json!({"type": "summary", "summary": "x"})), None);
    }
}
