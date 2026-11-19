# Pulse events and attribution

## `pulse.event.v1`

Every adapter produces the same event, stored one per line in `events/YYYY-MM-DD.jsonl` (UTC day) in Pulse's data directory:

```json
{"v":1,"t":1795051591,"source":"claude-code","session":"s1","project":"github.com/rowan/cs2-web","kind":"prompt_submitted","model":"claude-opus-4-5"}
```

| Field | Meaning |
|---|---|
| `v` | Protocol version, `1`. |
| `t` | Unix time in whole seconds (UTC). |
| `source` | Adapter id, such as `claude-code` or `codex`. |
| `session` | The harness's session id. Turns are followed within a session. |
| `project` | Normalized project id: the `origin` remote as `host/owner/repo` (credentials stripped), or `local/<folder>` without one. |
| `kind` | One of the kinds below. |
| `model` | Optional model name, when the harness reports it. |

| Kind | Who | Meaning |
|---|---|---|
| `session_started` | person | A coding agent session was opened. |
| `prompt_submitted` | person | The person sent a prompt; an agent turn starts. |
| `tool_decision` | person | The person approved or rejected an agent action. |
| `agent_activity` | agent | A tool call, message or edit by the agent. |
| `turn_ended` | agent | The agent finished and is waiting for the person. |
| `session_ended` | agent | The session closed. |

Events never contain prompt text, code, file paths or URLs.

Input activity is stored separately in `inputs/YYYY-MM-DD.jsonl`, one unix timestamp per line, each one a moment when the OS idle timer saw keyboard or mouse input.

## Attribution

A second is credited to project P when both hold:

1. **Present:** there was input, or a person event, in the last 120 seconds.
2. **Engaged with P:** a person event has happened in one of P's sessions, and the second falls within 600 seconds after any event in that session, or inside an agent turn (from `prompt_submitted` to `turn_ended`).

When several projects are engaged, the second goes to the project with the most recent person event. Seconds when an agent was working (inside a turn, or within 60 seconds of `agent_activity`) but the person was not present count as *agent-only* time, which is reported but never credited. Agent events in a session where no person has acted are never credited.

An agent session is **open** while its project is engaged. The screen recorder will run only while at least one session is open.

The thresholds are starting values to be tuned against reviewer decisions in a pilot. The rules live in `crates/pulse-core/src/attribution.rs`.

## Writing an adapter

An adapter maps what a harness writes, either hook payloads or transcript lines, to the event kinds above, reading only the fields it needs. Adapters live in `crates/pulse/src/adapters/` with tests built from real payloads. Prefer hooks, which report in real time, and fall back to transcripts, which need no setup.
