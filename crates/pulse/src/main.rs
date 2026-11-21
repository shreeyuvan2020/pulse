mod adapters;
mod daemon;
mod install;
mod paths;
mod presence;
mod project;
mod recorder;
mod report;
mod store;
mod submit;
mod sync;
mod tail;
mod terminal;
mod tray;
#[cfg(all(unix, not(target_os = "macos")))]
mod wayland;

use std::io::{self, BufRead, Read, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Local, NaiveDate};
use clap::{Parser, Subcommand, ValueEnum};

use crate::store::Store;

#[derive(Parser)]
#[command(
    name = "pulse",
    version,
    about = "Passive time tracking for building with AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run in the background, following coding agents and the idle timer,
    /// and recording the screen while an agent session is open.
    Daemon {
        /// Track time without recording the screen. Hours tracked this way
        /// cannot be verified.
        #[arg(long)]
        no_record: bool,
    },
    /// Record the screen for a fixed time and print the overhead, to measure
    /// how Pulse runs on this computer.
    Record {
        #[arg(long, default_value_t = 60)]
        seconds: u64,
        /// Where to write the recording (default: a `benchmark` folder in Pulse's data).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
    /// Never delete this project's recordings to make room. Use it for a
    /// project you plan to submit to a program.
    Protect { project: String },
    /// Allow this project's old recordings to be deleted again when space is needed.
    Unprotect { project: String },
    /// Check that local recordings still match their hash chain.
    Verify { dir: Option<std::path::PathBuf> },
    /// Called by a coding agent's hooks with the hook payload on stdin.
    #[command(hide = true)]
    Hook { adapter: Adapter },
    /// Terminal tracking: shell hooks record when commands start and end, and
    /// in which directory. Only the first word of a command is ever stored.
    Terminal {
        #[command(subcommand)]
        command: TerminalCommand,
    },
    /// Register Pulse's hooks in a coding agent.
    Install {
        adapter: Adapter,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Remove Pulse's hooks from a coding agent.
    Uninstall { adapter: Adapter },
    /// Show today's AI-assisted time.
    Today {
        #[arg(long)]
        json: bool,
    },
    /// Show AI-assisted time for a day (YYYY-MM-DD, local time).
    Report {
        date: NaiveDate,
        #[arg(long)]
        json: bool,
    },
    /// Print the raw events Pulse stored for a day, exactly as saved.
    Events { date: Option<NaiveDate> },
    /// Submit a project's recordings to a program so reviewers can see the
    /// work behind your hours. Shows what would be sent and asks first.
    Submit {
        /// The program to submit to, as the server lists it.
        program: String,
        /// The project, as `pulse status` and your profile name it.
        project: String,
        /// Only recordings from this day on (YYYY-MM-DD, local time).
        #[arg(long)]
        from: Option<NaiveDate>,
        /// Only recordings up to and including this day.
        #[arg(long)]
        to: Option<NaiveDate>,
        /// Leave out the segment starting at this unix time (repeat for more).
        #[arg(long)]
        skip: Vec<i64>,
        /// Show what would be submitted and stop.
        #[arg(long)]
        dry_run: bool,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Connect this computer to a Pulse server, so your time shows on your
    /// profile. Make a token on the server's page ("Connect a computer"); you
    /// are asked to paste it, or it is read from standard input.
    Login {
        /// Address of the server, like https://pulse.example.org
        server: String,
    },
    /// Show a tray icon with today's time, a link to the dashboard and Quit.
    /// On Linux it needs a StatusNotifier host, which most desktops provide.
    Tray,
    /// Disconnect this computer from the server. Time already sent stays there.
    Logout,
    /// Send new time to the server now and check the connection.
    Sync,
    /// Show where data lives and which agents and signals Pulse can see.
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
enum Adapter {
    ClaudeCode,
}

#[derive(Subcommand)]
enum TerminalCommand {
    /// Called by the shell hooks. Kept silent: tracking must never disturb a shell.
    #[command(hide = true)]
    Hook {
        #[arg(long)]
        kind: HookKind,
        #[arg(long)]
        cwd: std::path::PathBuf,
        #[arg(long)]
        command: String,
        #[arg(long)]
        session: String,
    },
    /// Print the snippet to add to this shell's config. Pass the shell to
    /// print another shell's snippet.
    Init {
        #[arg(long)]
        shell: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum HookKind {
    Start,
    End,
}

fn parse_shell(name: &str) -> Result<terminal::Shell, String> {
    match name {
        "bash" => Ok(terminal::Shell::Bash),
        "zsh" => Ok(terminal::Shell::Zsh),
        "fish" => Ok(terminal::Shell::Fish),
        other => Err(format!("{other} is not supported yet (bash, zsh, fish)")),
    }
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn main() -> ExitCode {
    // `pulse status | head` must not panic when the reader goes away: unix
    // tools stop quietly when their output pipe closes
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.to_string().contains("Broken pipe") {
            std::process::exit(0);
        }
        default_hook(info);
    }));
    let cli = Cli::parse();
    let store = Store::new(paths::data_dir());
    let Some(command) = cli.command else {
        // double-clicking Pulse.app runs the binary with no arguments: the
        // tray is the app. A bare `pulse` in a terminal still shows help.
        if is_app_bundle_path(&std::env::current_exe().unwrap_or_default()) {
            return match tray::run(&store) {
                Ok(()) => ExitCode::SUCCESS,
                Err(message) => {
                    eprintln!("pulse: {message}");
                    ExitCode::FAILURE
                }
            };
        }
        use clap::CommandFactory;
        let _ = Cli::command().print_help();
        return ExitCode::SUCCESS;
    };
    let result = match command {
        Command::Hook { adapter } => {
            // a hook must never slow down or break the agent, so failures stay silent
            let _ = run_hook(&store, adapter);
            return ExitCode::SUCCESS;
        }
        Command::Terminal { command } => match command {
            TerminalCommand::Hook {
                kind,
                cwd,
                command,
                session,
            } => {
                // the shell redirects errors, so nothing is printed either way
                terminal::hook(
                    &store,
                    matches!(kind, HookKind::End),
                    &cwd,
                    &command,
                    &session,
                )
                .map_err(|error| error.to_string())
            }
            TerminalCommand::Init { shell } => terminal_init(shell.as_deref()),
        },
        Command::Daemon { no_record } => {
            daemon::run(&store, !no_record).map_err(|error| error.to_string())
        }
        Command::Record { seconds, out } => benchmark(
            &store,
            out.unwrap_or_else(|| store.root().join("benchmark")),
            seconds,
        ),
        Command::Protect { project } => protect(&store, &project, true),
        Command::Unprotect { project } => protect(&store, &project, false),
        Command::Verify { dir } => verify(&dir.unwrap_or_else(|| store.root().join("recordings"))),
        Command::Install {
            adapter: Adapter::ClaudeCode,
            yes,
        } => install_claude(yes),
        Command::Uninstall {
            adapter: Adapter::ClaudeCode,
        } => uninstall_claude(),
        Command::Today { json } => {
            print_report(&store, Local::now().date_naive(), json);
            Ok(())
        }
        Command::Report { date, json } => {
            print_report(&store, date, json);
            Ok(())
        }
        Command::Events { date } => {
            let date = date.unwrap_or_else(|| Local::now().date_naive());
            let from = report::local_midnight(date);
            let to = report::local_midnight(date.succ_opt().expect("date in range"));
            for event in store.events(from, to) {
                println!(
                    "{}",
                    serde_json::to_string(&event).expect("events serialize")
                );
            }
            Ok(())
        }
        Command::Submit {
            program,
            project,
            from,
            to,
            skip,
            dry_run,
            yes,
        } => submit::run(
            &store,
            &submit::Options {
                program,
                project,
                from,
                to,
                skip,
                dry_run,
                yes,
            },
        ),
        Command::Login { server } => login(&store, &server),
        Command::Tray => tray::run(&store),
        Command::Logout => logout(&store),
        Command::Sync => sync_now(&store),
        Command::Status => {
            print_status(&store);
            Ok(())
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("pulse: {message}");
            ExitCode::FAILURE
        }
    }
}

fn terminal_init(shell: Option<&str>) -> Result<(), String> {
    let picked = shell.map(parse_shell).transpose()?;
    let shell = picked.or_else(terminal::detect).ok_or(
        "cannot tell which shell this is; pass it: pulse terminal init --shell bash|zsh|fish",
    )?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    print!("{}", terminal::snippet(exe.as_path(), shell));
    Ok(())
}

/// Whether this binary lives inside a macOS app bundle. That decides what
/// running it with no arguments means.
pub fn is_app_bundle_path(exe: &Path) -> bool {
    exe.to_string_lossy().contains(".app/Contents/MacOS/")
}

fn run_hook(store: &Store, adapter: Adapter) -> Result<(), Box<dyn std::error::Error>> {
    let mut input = String::new();
    io::stdin().take(4 << 20).read_to_string(&mut input)?;
    let payload: serde_json::Value = serde_json::from_str(&input)?;
    let event = match adapter {
        Adapter::ClaudeCode => adapters::claude_code::from_hook(&payload, now(), &project::resolve),
    };
    if let Some(event) = event {
        store.append_event(&event)?;
    }
    Ok(())
}

fn print_report(store: &Store, date: NaiveDate, json: bool) {
    let (from, to, report) = report::day_report(store, date, now());
    if json {
        println!("{:#}", report::to_json(date, from, to, &report));
    } else {
        report::print_text(date, &report);
    }
}

mod settings {
    use std::path::Path;
    use std::{fs, io};

    use serde_json::{Value, json};

    pub fn read(path: &Path) -> Result<Value, String> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|error| {
                format!(
                    "{} is not valid JSON ({error}); fix it first",
                    path.display()
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(json!({})),
            Err(error) => Err(format!("cannot read {}: {error}", path.display())),
        }
    }

    pub fn write(path: &Path, settings: &Value) -> Result<(), String> {
        let fail = |error: io::Error| format!("cannot write {}: {error}", path.display());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(fail)?;
        }
        if path.exists() {
            fs::copy(path, path.with_extension("json.pulse-backup")).map_err(fail)?;
        }
        let mut text = serde_json::to_string_pretty(settings).expect("settings serialize");
        text.push('\n');
        fs::write(path, text).map_err(fail)
    }
}

fn claude_settings_path() -> Result<std::path::PathBuf, String> {
    paths::claude_dir()
        .map(|dir| dir.join("settings.json"))
        .ok_or_else(|| "cannot find your home directory".into())
}

fn install_claude(yes: bool) -> Result<(), String> {
    let path = claude_settings_path()?;
    let mut settings = settings::read(&path)?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let command = install::hook_command(&exe.to_string_lossy());
    let before = settings.clone();
    install::install(&mut settings, &command)?;
    if settings == before {
        println!("Pulse's Claude Code hooks are already installed.");
        return Ok(());
    }
    println!("Pulse will add hooks to {}:", path.display());
    println!(
        "  events: {}",
        adapters::claude_code::HOOK_EVENTS.join(", ")
    );
    println!("  command: {command}");
    println!(
        "The hooks record when you prompt and when the agent works. They never read your prompts or code."
    );
    if !yes && !confirm("Continue?")? {
        println!("Nothing changed.");
        return Ok(());
    }
    settings::write(&path, &settings)?;
    println!(
        "Installed. A backup of the previous file is next to it (settings.json.pulse-backup)."
    );
    Ok(())
}

fn uninstall_claude() -> Result<(), String> {
    let path = claude_settings_path()?;
    let mut settings = settings::read(&path)?;
    if !install::installed(&settings) {
        println!("Pulse's Claude Code hooks are not installed.");
        return Ok(());
    }
    install::uninstall(&mut settings);
    settings::write(&path, &settings)?;
    println!("Removed Pulse's hooks from {}.", path.display());
    Ok(())
}

fn confirm(question: &str) -> Result<bool, String> {
    print!("{question} [y/N] ");
    io::stdout().flush().map_err(|error| error.to_string())?;
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn print_wayland_status() {
    if !wayland::is_session() {
        return;
    }
    match wayland::client() {
        Ok(client) => println!(
            "Wayland: helper running, idle timer {}",
            client.idle_backend().as_deref().unwrap_or("not answering")
        ),
        Err(reason) => println!("Wayland: {reason}"),
    }
}

#[cfg(not(all(unix, not(target_os = "macos"))))]
fn print_wayland_status() {}

fn print_status(store: &Store) {
    println!("Data: {}", store.root().display());
    match presence::IdleTimer::new().idle_seconds() {
        Some(seconds) => println!("Idle timer: available (last input {seconds}s ago)"),
        None => println!("Idle timer: unavailable, prompts are used as the sign you are present"),
    }
    print_wayland_status();
    let exists = |path: Option<&Path>| path.is_some_and(Path::exists);
    let claude = paths::claude_dir();
    let hooks = claude
        .as_ref()
        .and_then(|dir| settings::read(&dir.join("settings.json")).ok())
        .is_some_and(|settings| install::installed(&settings));
    let claude_state = match (exists(claude.as_deref()), hooks) {
        (_, true) => "hooks installed",
        (true, false) => {
            "found, reading transcripts (run `pulse install claude-code` for real-time hooks)"
        }
        (false, false) => "not found",
    };
    println!("Claude Code: {claude_state}");
    let recordings = store.root().join("recordings");
    let usage = recorder::storage::usage(&recordings);
    let cap = recorder::storage::format_bytes(recorder::storage::cap_bytes());
    println!(
        "Recordings: {} in {} segments (limit {cap}, oldest videos are deleted first{})",
        recorder::storage::format_bytes(usage.bytes),
        usage.segments,
        if usage.pruned > 0 {
            format!("; {} already deleted", usage.pruned)
        } else {
            String::new()
        }
    );
    let protected = recorder::storage::protected(&recordings);
    if !protected.is_empty() {
        println!(
            "Protected from deletion: {} ({})",
            protected.iter().cloned().collect::<Vec<_>>().join(", "),
            recorder::storage::format_bytes(usage.protected_bytes)
        );
    }
    if let Some(free) = recorder::storage::free_bytes(store.root()) {
        println!("Free disk space: {}", recorder::storage::format_bytes(free));
    }
    match sync::load(store.root()) {
        None => println!(
            "Server: not connected (run `pulse login <server>` to show your time on a profile)"
        ),
        Some(connection) => println!(
            "Server: {} (last upload {})",
            connection.server,
            match sync::last_upload(store.root()) {
                Some(at) => format!("{} ago", report::format_duration(now() - at)),
                None => "never".into(),
            }
        ),
    }
    let codex = paths::codex_dir().map(|dir| dir.join("sessions"));
    println!(
        "Codex: {}",
        if exists(codex.as_deref()) {
            "found, reading session files"
        } else {
            "not found"
        }
    );
}

fn benchmark(store: &Store, dir: std::path::PathBuf, seconds: u64) -> Result<(), String> {
    let mut recorder = recorder::Recorder::open(dir.clone(), &store.root().join("codec"))?;
    let mut idle = presence::IdleTimer::new();
    let projects = std::collections::BTreeSet::from(["benchmark".to_owned()]);
    println!("Recording {seconds}s into {}", dir.display());
    let started = std::time::Instant::now();
    for _ in 0..seconds {
        let input = idle.idle_seconds() == Some(0);
        recorder
            .record(now(), input, &projects)
            .map_err(|error| error.to_string())?;
        std::thread::sleep(daemon::until_next_second());
    }
    recorder.finish().map_err(|error| error.to_string())?;
    recorder.release_screen();
    let wall = started.elapsed().as_secs_f64();
    let stats = recorder.stats;
    let megabytes = stats.bytes as f64 / 1e6;
    println!("Frames: {}", stats.frames);
    println!(
        "Video: {megabytes:.2} MB, about {:.0} MB per recorded hour",
        megabytes * 3600.0 / wall
    );
    println!(
        "Work per frame: {:.1} ms on average ({:.1}% of one CPU core)",
        stats.busy.as_secs_f64() * 1000.0 / stats.frames.max(1) as f64,
        stats.busy.as_secs_f64() * 100.0 / wall
    );
    Ok(())
}

fn verify(dir: &Path) -> Result<(), String> {
    let verified = recorder::verify(dir)?;
    let seconds: i64 = verified
        .links
        .iter()
        .map(|link| link.end - link.start)
        .sum();
    println!(
        "{} segments, {} of recording, all intact.",
        verified.links.len(),
        report::format_duration(seconds)
    );
    if verified.pruned > 0 {
        println!(
            "{} of them had their video deleted to save space; their timelines still verify.",
            verified.pruned
        );
    }
    Ok(())
}

fn protect(store: &Store, project: &str, on: bool) -> Result<(), String> {
    let recordings = store.root().join("recordings");
    let changed = recorder::storage::set_protected(&recordings, project, on)
        .map_err(|error| error.to_string())?;
    match (on, changed) {
        (true, true) => println!(
            "{project}: its recordings will not be deleted to make room. Footage Pulse already deleted cannot come back."
        ),
        (true, false) => println!("{project} is already protected."),
        (false, true) => {
            println!("{project}: its old recordings can be deleted again when space is needed.")
        }
        (false, false) => println!("{project} was not protected."),
    }
    Ok(())
}

fn login(store: &Store, server: &str) -> Result<(), String> {
    let server = sync::normalize_server(server)?;
    eprint!("Paste the token from {server}: ");
    io::stderr().flush().map_err(|error| error.to_string())?;
    let mut token = String::new();
    io::stdin()
        .lock()
        .read_line(&mut token)
        .map_err(|error| error.to_string())?;
    let connection = sync::Connection {
        server,
        token: token.trim().to_owned(),
    };
    if !connection.token.starts_with("pulse_") {
        return Err("that does not look like a Pulse token (they start with pulse_)".into());
    }
    sync::verify(&connection).map_err(|error| error.to_string())?;
    // a new account starts from scratch, so time that went to another one is not copied over
    if sync::load(store.root()).is_some_and(|old| old != connection) {
        sync::remove(store.root()).map_err(|error| error.to_string())?;
    }
    sync::save(store.root(), &connection).map_err(|error| error.to_string())?;
    println!(
        "Connected. The running daemon picks this up within a minute; `pulse sync` sends time right away."
    );
    Ok(())
}

fn logout(store: &Store) -> Result<(), String> {
    match sync::remove(store.root()).map_err(|error| error.to_string())? {
        true => println!(
            "Disconnected. Time already sent stays on the server; revoke this computer there to be sure."
        ),
        false => println!("This computer was not connected."),
    }
    Ok(())
}

fn sync_now(store: &Store) -> Result<(), String> {
    let connection =
        sync::load(store.root()).ok_or("not connected, run `pulse login <server>` first")?;
    let outcome = sync::run_once(store.root(), &connection, now(), true)
        .map_err(|error| error.to_string())?;
    println!("Sent {} entries to {}.", outcome.sent, connection.server);
    for reason in outcome.rejected {
        println!("  the server refused one: {reason}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_app_bundle_path;
    use std::path::Path;

    #[test]
    fn only_paths_inside_an_app_bundle_start_the_tray_with_no_arguments() {
        assert!(is_app_bundle_path(Path::new(
            "/Applications/Pulse.app/Contents/MacOS/pulse"
        )));
        assert!(!is_app_bundle_path(Path::new("/usr/local/bin/pulse")));
        assert!(!is_app_bundle_path(Path::new(
            "/home/u/not-an-app/Contents/MacOS/pulse"
        )));
        assert!(!is_app_bundle_path(Path::new("pulse")));
    }
}
