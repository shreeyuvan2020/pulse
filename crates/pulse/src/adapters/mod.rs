//! Adapters turn what a coding agent writes (hook payloads, transcript lines)
//! into `pulse.event.v1` events. They read only the fields they need and never
//! copy prompt text, code or paths into an event.

pub mod claude_code;
pub mod codex;
pub mod opencode;

use std::path::Path;

use chrono::DateTime;
use serde_json::Value;

/// Resolves a working directory to a project id.
pub type Resolve<'a> = &'a dyn Fn(&Path) -> String;

fn parse_time(value: Option<&Value>) -> Option<i64> {
    DateTime::parse_from_rfc3339(value?.as_str()?)
        .ok()
        .map(|time| time.timestamp())
}

fn text<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key)?.as_str()
}
