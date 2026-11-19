//! OpenAI Codex CLI: session files under `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`.
//! Only the first line of a session carries its id and directory, so the
//! reader keeps that context per file.

use std::path::Path;

use pulse_core::{Event, EventKind, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Resolve, parse_time, text};

pub const SOURCE: &str = "codex";

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: Option<String>,
    pub project: Option<String>,
    pub model: Option<String>,
}

/// Reads one line, updating `session` and returning the event it describes.
pub fn read_line(session: &mut Session, line: &Value, resolve: Resolve) -> Option<Event> {
    let payload = line.get("payload")?;
    let kind = match (text(line, "type")?, text(payload, "type")) {
        ("session_meta", _) => {
            session.id = text(payload, "id").map(str::to_owned).or(session.id.take());
            if let Some(cwd) = text(payload, "cwd") {
                session.project = Some(resolve(Path::new(cwd)));
            }
            EventKind::SessionStarted
        }
        ("turn_context", _) => {
            if let Some(cwd) = text(payload, "cwd") {
                session.project = Some(resolve(Path::new(cwd)));
            }
            if let Some(model) = text(payload, "model") {
                session.model = Some(model.to_owned());
            }
            return None;
        }
        ("event_msg", Some("user_message")) => EventKind::PromptSubmitted,
        ("event_msg", Some("task_complete")) => EventKind::TurnEnded,
        ("event_msg", Some("agent_message" | "agent_reasoning")) => EventKind::AgentActivity,
        ("response_item", Some("function_call" | "custom_tool_call" | "local_shell_call")) => {
            EventKind::AgentActivity
        }
        _ => return None,
    };
    Some(Event {
        v: PROTOCOL_VERSION,
        t: parse_time(line.get("timestamp"))?,
        source: SOURCE.into(),
        session: session.id.clone()?,
        project: session.project.clone()?,
        kind,
        model: session
            .model
            .clone()
            .filter(|_| kind != EventKind::SessionStarted),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn follows_a_session_from_its_first_line() {
        let lines = [
            json!({"timestamp": "2026-11-19T01:00:00Z", "type": "session_meta", "payload": {"id": "c1", "cwd": "/home/rowan/cs2-web", "originator": "codex_cli_rs"}}),
            json!({"timestamp": "2026-11-19T01:00:01Z", "type": "turn_context", "payload": {"cwd": "/home/rowan/cs2-web", "model": "gpt-5-codex"}}),
            json!({"timestamp": "2026-11-19T01:00:02Z", "type": "response_item", "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "<environment_context>"}]}}),
            json!({"timestamp": "2026-11-19T01:00:02Z", "type": "event_msg", "payload": {"type": "user_message", "message": "port the smoke grenades"}}),
            json!({"timestamp": "2026-11-19T01:00:09Z", "type": "response_item", "payload": {"type": "function_call", "name": "shell", "arguments": "{}"}}),
            json!({"timestamp": "2026-11-19T01:00:30Z", "type": "event_msg", "payload": {"type": "token_count"}}),
            json!({"timestamp": "2026-11-19T01:00:40Z", "type": "event_msg", "payload": {"type": "task_complete", "last_agent_message": "done"}}),
        ];
        let mut session = Session::default();
        let resolve = |path: &Path| format!("resolved:{}", path.display());

        let events: Vec<(i64, EventKind, Option<String>)> = lines
            .iter()
            .filter_map(|line| read_line(&mut session, line, &resolve))
            .map(|event| {
                assert_eq!(event.session, "c1");
                assert_eq!(event.project, "resolved:/home/rowan/cs2-web");
                (event.t % 60, event.kind, event.model)
            })
            .collect();

        let model = Some("gpt-5-codex".to_owned());
        assert_eq!(
            events,
            vec![
                (0, EventKind::SessionStarted, None),
                (2, EventKind::PromptSubmitted, model.clone()),
                (9, EventKind::AgentActivity, model.clone()),
                (40, EventKind::TurnEnded, model),
            ]
        );
    }
}
