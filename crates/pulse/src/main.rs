mod adapters;
mod daemon;
mod install;
mod paths;
mod presence;
mod project;
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
    /// Run in the background, following coding agents and the idle timer.
    Daemon,
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
        Command::Daemon => daemon::run(&store).map_err(|error| error.to_string()),
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
