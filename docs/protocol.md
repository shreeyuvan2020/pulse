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

## Recordings

While a session is open, the daemon records into `recordings/` in Pulse's data directory: `<start>.mp4` (5 minutes at 1 fps), `<start>.timeline.json` (`[t, change, input]` per second, with `change` the share of 160×90 brightness cells that changed, per thousand) and one line per segment in `chain.jsonl`. Each link is `sha256("pulse.chain.v1" || previous || start || end || sha256(video) || timeline_hash)`, starting from 32 zero bytes; the exact byte layout is in `crates/pulse-core/src/chain.rs`. If the folder grows past its limit, the oldest `<start>.mp4` files are deleted but their timeline sidecars and `chain.jsonl` lines are kept, so links still verify and only the video of that stretch is lost. Projects listed in `recordings/protected.json` (a JSON array of project ids) are skipped when deleting: a segment is protected if any project in its chain line is. A server that receives each link as it is made can later check that a submitted recording existed at that time and was not changed.

## The terminal adapter

`pulse terminal init` prints a snippet for bash, zsh or fish. The shell calls `pulse terminal hook --kind start|end --cwd "$PWD" --command <first word> --session <pid>` when a command starts and when it ends. The hooks are silent and can never break the shell. Terminal commands open and extend engagement windows like prompts, but the `terminal` source never counts as agent time: a dev server left running while you are away does not become "agent work". Command time reaches the server through the same per-slot credits, with `terminal` in the sources list and no prompts counted.

## Writing an adapter

An adapter maps what a harness writes, either hook payloads or transcript lines, to the event kinds above, reading only the fields it needs. Adapters live in `crates/pulse/src/adapters/` with tests built from real payloads. Prefer hooks, which report in real time, and fall back to transcripts, which need no setup.
