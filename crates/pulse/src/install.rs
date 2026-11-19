//! Adds and removes Pulse's hooks in Claude Code's `settings.json`, leaving
//! every other setting and hook as it was.

use serde_json::{Map, Value, json};

use crate::adapters::claude_code::HOOK_EVENTS;

const COMMAND_SUFFIX: &str = " hook claude-code";

pub fn hook_command(exe: &str) -> String {
    format!("\"{exe}\"{COMMAND_SUFFIX}")
}

fn is_ours(hook: &Value) -> bool {
    hook.get("command")
        .and_then(Value::as_str)
        .is_some_and(|command| command.ends_with(COMMAND_SUFFIX))
}

pub fn installed(settings: &Value) -> bool {
    settings
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .any(|group| {
                    group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|list| list.iter().any(is_ours))
                })
        })
}

/// Removes Pulse's hooks, dropping groups and events that end up empty.
pub fn uninstall(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    for groups in hooks.values_mut().filter_map(Value::as_array_mut) {
        for group in groups.iter_mut() {
            if let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                list.retain(|hook| !is_ours(hook));
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|list| !list.is_empty())
        });
    }
    hooks.retain(|_, groups| groups.as_array().is_none_or(|list| !list.is_empty()));
    if hooks.is_empty()
        && let Some(object) = settings.as_object_mut()
    {
        object.remove("hooks");
    }
}

/// Installs Pulse's hooks, replacing any older Pulse entries (for example
/// ones pointing at a previous install location).
pub fn install(settings: &mut Value, command: &str) -> Result<(), String> {
    uninstall(settings);
    let object = settings
        .as_object_mut()
        .ok_or("settings.json is not a JSON object")?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("\"hooks\" in settings.json is not an object")?;
    for event in HOOK_EVENTS {
        let mut group = json!({"hooks": [{"type": "command", "command": command, "timeout": 5}]});
        if *event == "PostToolUse" {
            group["matcher"] = json!("*");
        }
        hooks
            .entry(*event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| format!("hooks.{event} in settings.json is not a list"))?
            .push(group);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent_and_uninstall_restores_other_settings() {
        let original = json!({
            "model": "opus",
            "hooks": {"PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "prettier --write"}]}]}
        });
        let mut settings = original.clone();

        install(&mut settings, &hook_command("/old/pulse")).unwrap();
        install(&mut settings, &hook_command("/new/pulse")).unwrap();

        assert!(installed(&settings));
        let post_tool_use = settings["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post_tool_use.len(), 2);
        assert_eq!(
            post_tool_use[1]["hooks"][0]["command"],
            "\"/new/pulse\" hook claude-code"
        );
        assert!(!settings.to_string().contains("/old/pulse"));
        assert_eq!(
            settings["hooks"].as_object().unwrap().len(),
            HOOK_EVENTS.len()
        );

        uninstall(&mut settings);
        assert!(!installed(&settings));
        assert_eq!(settings, original);
    }
}
