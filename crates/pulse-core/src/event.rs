use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u8 = 1;

/// One thing that happened in a coding agent, as written to the local spool.
///
/// Events never carry prompt text, file paths or code. `project` is the
/// normalized project id (for example `github.com/hackclub/pulse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub v: u8,
    /// Unix time in whole seconds (UTC).
    pub t: i64,
    /// Adapter that produced the event, such as `claude-code` or `codex`.
    pub source: String,
    /// Harness session id, so turns can be followed within one session.
    pub session: String,
    pub project: String,
    pub kind: EventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// The source of events the shell integration writes. Anything else is an
/// agent.
pub const TERMINAL_SOURCE: &str = "terminal";

/// Whether a source names a coding agent rather than a terminal.
pub fn source_is_agent(source: &str) -> bool {
    source != TERMINAL_SOURCE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The person opened a coding agent session.
    SessionStarted,
    /// The person sent a prompt. This also starts an agent turn.
    PromptSubmitted,
    /// The person ran a command in a terminal. Only the command's first word
    /// is ever stored, never its arguments.
    CommandSubmitted,
    /// A terminal command finished.
    CommandEnded,
    /// The person approved or rejected something the agent asked to do.
    ToolDecision,
    /// The agent did something: a tool call, a message, a file edit.
    AgentActivity,
    /// The agent finished its turn and is waiting for the person.
    TurnEnded,
    SessionEnded,
}

impl EventKind {
    /// Whether this event can only come from a person acting.
    pub fn is_human(self) -> bool {
        matches!(
            self,
            EventKind::SessionStarted
                | EventKind::PromptSubmitted
                | EventKind::CommandSubmitted
                | EventKind::ToolDecision
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_stable() {
        let event = Event {
            v: PROTOCOL_VERSION,
            t: 1_763_510_400,
            source: "claude-code".into(),
            session: "abc".into(),
            project: "github.com/hackclub/pulse".into(),
            kind: EventKind::PromptSubmitted,
            model: None,
        };
        assert_eq!(
            serde_json::to_string(&event).unwrap(),
            r#"{"v":1,"t":1763510400,"source":"claude-code","session":"abc","project":"github.com/hackclub/pulse","kind":"prompt_submitted"}"#
        );
        let command = Event {
            kind: EventKind::CommandSubmitted,
            ..event
        };
        assert_eq!(
            serde_json::to_string(&command).unwrap(),
            r#"{"v":1,"t":1763510400,"source":"claude-code","session":"abc","project":"github.com/hackclub/pulse","kind":"command_submitted"}"#
        );
        assert!(EventKind::CommandSubmitted.is_human());
        assert!(!EventKind::CommandEnded.is_human());
    }
}
