# Pulse

Passive time tracking for building with AI coding agents, made by Hack Club.

Pulse counts the time **you** spend in the loop with a coding agent: prompting, reading what it did, reviewing, testing and steering. Agent time while you are away is shown separately and never counted. It runs in the background on Windows, macOS and Linux, and is built to stay light on old laptops.

> Status: early development. This repository currently contains the desktop tracker (agent detection, idle timer, local timeline and reports). The screen recorder, the server (accounts, profiles, real-time dashboard) and submissions come next.

## How it works

- **Adapters** turn what coding agents already write into events: Claude Code hooks or transcripts, and Codex CLI session files. More adapters will follow, and any tool can send events itself (see [docs/protocol.md](docs/protocol.md)).
- The **OS idle timer** tells Pulse whether you are at the computer. It is a single "seconds since last input" number: Pulse never sees which keys you press.
- **Attribution** credits a second to a project when you are present and its agent session is open, giving the second to the project you touched most recently, so parallel agents never multiply your hours. The rules are in [docs/protocol.md](docs/protocol.md#attribution).

Everything stays on your computer as plain JSON Lines files you can read (`pulse events`). Events never contain prompt text, code or file paths.

## Usage

Build from source with Rust 1.89 or newer:

```sh
cargo build --release
./target/release/pulse status                 # what Pulse can see
./target/release/pulse install claude-code    # optional: real-time Claude Code hooks
./target/release/pulse daemon                 # run the tracker
./target/release/pulse today                  # today's AI-assisted time
```

Other commands: `pulse report YYYY-MM-DD`, `pulse today --json`, `pulse events [YYYY-MM-DD]`, `pulse uninstall claude-code`.

Data lives in `%LOCALAPPDATA%\pulse` on Windows, `~/Library/Application Support/pulse` on macOS and `~/.local/share/pulse` on Linux (override with `PULSE_HOME`).

## Development

```sh
cargo test
cargo clippy --all-targets
cargo fmt --check
```

`crates/pulse-core` holds the event model and attribution rules, with no I/O, so the future server can share them. `crates/pulse` is the desktop binary.

## License

[GNU Affero General Public License v3.0](LICENSE).
