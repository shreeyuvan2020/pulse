# Pulse

Passive time tracking for building with AI coding agents, made by Hack Club.

Pulse counts the time **you** spend in the loop with a coding agent: prompting, reading what it did, reviewing, testing and steering. Agent time while you are away is shown separately and never counted. It runs in the background on Windows, macOS and Linux, and is built to stay light on old laptops.

> Status: early development. This repository currently contains the desktop tracker (agent detection, idle timer, local timeline and reports) and the screen recorder (X11, macOS ScreenCaptureKit and Windows DXGI Desktop Duplication capture, Cisco's OpenH264 software encoding so far). Windows capture compiles and is smoke-tested in CI but has not been tried on real hardware yet. Wayland capture, hardware encoders, the server (accounts, profiles, real-time dashboard) and submissions come next.

## How it works

- **Adapters** turn what coding agents already write into events: Claude Code hooks or transcripts, and Codex CLI session files. More adapters will follow, and any tool can send events itself (see [docs/protocol.md](docs/protocol.md)).
- The **OS idle timer** tells Pulse whether you are at the computer. It is a single "seconds since last input" number: Pulse never sees which keys you press.
- **Attribution** credits a second to a project when you are present and its agent session is open, giving the second to the project you touched most recently, so parallel agents never multiply your hours. The rules are in [docs/protocol.md](docs/protocol.md#attribution).

- The **recorder** captures one frame per second while a session is open and you are active (typing, or sending prompts and commands), scaled to fit 1280×720, as 5-minute H.264 MP4 segments. It pauses after two minutes of no input and no action from you, and resumes when you act again; the agent working by itself does not keep it recording. Each segment gets a per-second activity timeline (screen change score and an input yes/no) and a link in a SHA-256 hash chain (`pulse verify` checks it). `pulse daemon --no-record` tracks time without recording, but those hours cannot be verified.

- **Video codec:** Pulse does not build or ship H.264 itself. On first recording it downloads Cisco's prebuilt OpenH264 2.7.0 from `ciscobinary.openh264.org` into its data folder and loads it only if its SHA-256 matches a known Cisco release. OpenH264 Video Codec provided by Cisco Systems, Inc.

- **Storage:** recordings are about 5-25 MB per recorded hour. Once the recordings folder passes 5 GB (`PULSE_MAX_STORAGE_GB` changes it), the oldest segment videos are deleted first. Each deleted segment keeps its hash-chain line and timeline (a few KB), so `pulse verify` still passes; that stretch just can no longer be submitted. Footage of a project you plan to submit can be protected with `pulse protect <project>`: it is never deleted to make room (the folder can then stay over the limit, and Pulse says so), until `pulse unprotect <project>`. Programs will do this automatically once you link a project. Pulse also stops starting new segments when less than 1 GiB of disk is free, and `pulse status` shows usage and free space.

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

Other commands: `pulse tray` (an icon in the system tray: today's time in the menu, a link to your dashboard, and Quit), `pulse terminal init` (tracks commands in bash, zsh or fish: add the snippet it prints to your shell config; only the first word of each command is stored, never arguments), `pulse protect <project>`, `pulse unprotect <project>`, `pulse record --seconds 60` (measure recording overhead on this computer), `pulse verify`, `pulse report YYYY-MM-DD`, `pulse today --json`, `pulse events [YYYY-MM-DD]`, `pulse uninstall claude-code`.

Data lives in `%LOCALAPPDATA%\pulse` on Windows, `~/Library/Application Support/pulse` on macOS and `~/.local/share/pulse` on Linux (override with `PULSE_HOME`).

## Server

`server/` is the Pulse server: accounts (Hack Club Auth), public-by-default profiles, projects, per-computer API tokens, and a dashboard that updates live (server-sent events) as time is logged. It stores only credited time per 5-minute slot, never recordings, and never lets one person log more than 300 seconds in a 300-second slot, whatever the clients send. Project names stay hidden until the owner shows them.

```sh
export DATABASE_URL=postgres://user@localhost/pulse PULSE_BASE_URL=http://localhost:8080
export PULSE_DEV_LOGIN=1     # sign in as any name; only works when PULSE_BASE_URL is local
# or real sign-in: HCA_CLIENT_ID, HCA_CLIENT_SECRET (HCA_BASE_URL defaults to the staging instance, https://hca.dinosaurbbq.org)
cargo run -p pulse-server
```

### GitHub

A GitHub App gives two things, each switched on by its own settings:

- **Connect GitHub** (`GITHUB_CLIENT_ID`, `GITHUB_CLIENT_SECRET`, callback `<server>/auth/github/callback`) proves which GitHub account is yours. Pulse reads your numeric id and keeps no token.
- **Push times** (`GITHUB_WEBHOOK_SECRET`, webhook URL `<server>/api/github/webhook`, event `push`, permission Contents: read; `GITHUB_APP_SLUG` adds an install link): a push is recorded only if its signature checks out and its sender is a connected person. The time comes from GitHub's notification, never from commit dates, which anyone can set.

A project named `github.com/owner/repo` links to the repository and shows its push count and last push, but only when its owner has made the project public, like its name.

### Submitting hours to a program

`pulse submit <program> <project>` uploads a project's recordings so a program's reviewers can see the work behind the hours. It shows what would be sent (add `--dry-run` to stop there), flags segments that also covered other projects, lets you leave segments out with `--skip`, and asks before uploading. The daemon commits each recorded segment's hash-chain link to the server as it records, so the server can later check that an upload is the recording that existed at that time. Hours of a segment can be submitted once; withdrawing a submission on your profile deletes its recordings. Servers enable it with `PULSE_STORAGE_DIR` and `PULSE_PROGRAM_KEYS`. What a program's review tool reads is described in [docs/evidence.md](docs/evidence.md).

Tests need a Postgres they may create databases in: `PULSE_TEST_DATABASE_URL=postgres://user@localhost/postgres cargo test -p pulse-server`. 

To send your time: make a token on your profile page ("Connect a computer"), run `pulse login <server>` and paste it, then keep `pulse daemon` running. It uploads credited time every minute (per project and 5-minute slot, with prompt counts and tool names; never recordings, prompts or paths). `pulse sync` sends right away, `pulse logout` disconnects. Plain `http://` is only accepted for localhost.

## Installing

- **Linux:** `curl -fsSL https://raw.githubusercontent.com/shreeyuvan2020/pulse/main/packaging/install.sh | bash` installs the latest release into `~/.local/bin`. Debian and Ubuntu can use the `.deb` from the release page; Arch users can build `packaging/aur/pulse-bin` from the AUR.
- **macOS:** the same one-liner installs `Pulse.app` into `~/Applications` (move it to `/Applications` to keep it in the menu bar every day), or `brew install pulse` from a tap of `packaging/homebrew/pulse.rb`.
- **Windows:** the release zip on the release page, or `winget install HackClub.Pulse` / `scoop install pulse` once the manifests in `packaging/windows` are published with their hashes.

Pushing a `v*` tag builds the tarball, the `.deb`, `Pulse.app` and the Windows zip through `.github/workflows/release.yml` and attaches them to a GitHub Release. The binaries are not signed yet: Windows SmartScreen and macOS Gatekeeper will warn until SignPath signing and Apple notarization are set up.

## Development

```sh
cargo test
cargo clippy --all-targets
cargo fmt --check
```

`crates/pulse-core` holds the event model and attribution rules, with no I/O, so the future server can share them. `crates/pulse` is the desktop binary.

## License

[GNU Affero General Public License v3.0](LICENSE).
