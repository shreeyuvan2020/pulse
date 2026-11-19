mod adapters;
mod daemon;
mod install;
mod paths;
mod presence;
mod project;
mod recorder;
mod report;
mod store;
mod tail;

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
    command: Command,
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
    /// Show where data lives and which agents and signals Pulse can see.
    Status,
}

#[derive(Clone, Copy, ValueEnum)]
enum Adapter {
    ClaudeCode,
}

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let store = Store::new(paths::data_dir());
    let result = match cli.command {
        Command::Hook { adapter } => {
            // a hook must never slow down or break the agent, so failures stay silent
            let _ = run_hook(&store, adapter);
            return ExitCode::SUCCESS;
        }
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

fn print_status(store: &Store) {
    println!("Data: {}", store.root().display());
    match presence::IdleTimer::new().idle_seconds() {
        Some(seconds) => println!("Idle timer: available (last input {seconds}s ago)"),
        None => println!("Idle timer: unavailable, prompts are used as the sign you are present"),
    }
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
