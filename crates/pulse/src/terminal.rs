//! The terminal adapter: shell hooks that tell Pulse when a command starts
//! and ends, and in which directory. Like terminal-wakatime, the shell calls
//! back into Pulse; unlike it, only the command's first word is ever stored
//! (never arguments, never output), and the recording rules still decide what
//! counts.
//!
//! `pulse terminal init` prints the snippet for the current shell. A terminal
//! command is a sign that the person is working, not a sign that an agent is:
//! a dev server left running never counts as agent time, and a command's
//! project comes from the directory it was run in.

use std::io;
use std::path::Path;

use pulse_core::{Event, EventKind, PROTOCOL_VERSION, TERMINAL_SOURCE};

use crate::now;
use crate::project;
use crate::store::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    /// Which file the snippet belongs in.
    pub fn config_file(self) -> &'static str {
        match self {
            Shell::Bash => "~/.bashrc",
            Shell::Zsh => "~/.zshrc",
            Shell::Fish => "~/.config/fish/config.fish",
        }
    }
}

/// Decides the shell from the same signals shells set about themselves, so it
/// can be tested without touching the environment.
pub fn detect_from(
    zsh_version: Option<&str>,
    bash_version: Option<&str>,
    fish_pid: Option<&str>,
    shell_path: Option<&str>,
) -> Option<Shell> {
    if fish_pid.is_some() {
        return Some(Shell::Fish);
    }
    if zsh_version.is_some() {
        return Some(Shell::Zsh);
    }
    if bash_version.is_some() {
        return Some(Shell::Bash);
    }
    let name = Path::new(shell_path?).file_name()?.to_string_lossy();
    match name.as_ref() {
        "fish" => Some(Shell::Fish),
        "zsh" => Some(Shell::Zsh),
        "bash" => Some(Shell::Bash),
        _ => None,
    }
}

pub fn detect() -> Option<Shell> {
    detect_from(
        std::env::var_os("ZSH_VERSION")
            .as_deref()
            .and_then(|v| v.to_str()),
        std::env::var_os("BASH_VERSION")
            .as_deref()
            .and_then(|v| v.to_str()),
        std::env::var_os("fish_pid")
            .as_deref()
            .and_then(|v| v.to_str()),
        std::env::var_os("SHELL")
            .as_deref()
            .and_then(|v| v.to_str()),
    )
}

/// Quotes a word for a single-quoted shell string.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The snippet to add to the shell's config, using the absolute path of this
/// binary so PATH does not matter. Every hook call is silenced and ignored on
/// failure: tracking must never disturb a shell.
pub fn snippet(exe: &Path, shell: Shell) -> String {
    let exe = quote(&exe.to_string_lossy());
    let header = format!(
        "# Pulse terminal tracking: add this to {} (then open a new shell).\n# Only the first word of each command is recorded, never arguments.",
        shell.config_file()
    );
    match shell {
        Shell::Bash => format!(
            "{header}\n\
             _pulse_exe={exe}\n\
             _pulse_cmd=''\n\
             _pulse_preexec() {{\n\
             \x20 case \"$BASH_COMMAND\" in *terminal\\ hook*) return;; esac\n\
             \x20 _pulse_cmd=${{BASH_COMMAND%%[[:space:]]*}}\n\
             \x20 [ -n \"$_pulse_cmd\" ] && \"$_pulse_exe\" terminal hook --kind start --cwd \"$PWD\" --command \"$_pulse_cmd\" --session \"$$\" 2>/dev/null\n\
             }}\n\
             _pulse_precmd() {{\n\
             \x20 [ -n \"$_pulse_cmd\" ] && \"$_pulse_exe\" terminal hook --kind end --cwd \"$PWD\" --command \"$_pulse_cmd\" --session \"$$\" 2>/dev/null\n\
             \x20 _pulse_cmd=''\n\
             }}\n\
             trap '_pulse_preexec' DEBUG\n\
             PROMPT_COMMAND=\"_pulse_precmd${{PROMPT_COMMAND:+;$PROMPT_COMMAND}}\"\n"
        ),
        Shell::Zsh => format!(
            "{header}\n\
             _pulse_exe={exe}\n\
             _pulse_cmd=''\n\
             _pulse_preexec() {{\n\
             \x20 [[ $1 == *terminal\\ hook* ]] && return\n\
             \x20 _pulse_cmd=${{${{(z)1}}[1]}}\n\
             \x20 [ -n \"$_pulse_cmd\" ] && \"$_pulse_exe\" terminal hook --kind start --cwd \"$PWD\" --command \"$_pulse_cmd\" --session \"$$\" 2>/dev/null\n\
             }}\n\
             _pulse_precmd() {{\n\
             \x20 [ -n \"$_pulse_cmd\" ] && \"$_pulse_exe\" terminal hook --kind end --cwd \"$PWD\" --command \"$_pulse_cmd\" --session \"$$\" 2>/dev/null\n\
             \x20 _pulse_cmd=''\n\
             }}\n\
             autoload -Uz add-zsh-hook 2>/dev/null\n\
             add-zsh-hook preexec _pulse_preexec 2>/dev/null\n\
             add-zsh-hook precmd _pulse_precmd 2>/dev/null\n"
        ),
        Shell::Fish => format!(
            "{header}\n\
             set -g _pulse_exe {exe}\n\
             set -g _pulse_cmd ''\n\
             function _pulse_preexec --on-event fish_preexec\n\
             \x20 string match -q '*terminal hook*' -- $argv[1]; and return\n\
             \x20 set -g _pulse_cmd $argv[1]\n\
             \x20 test -n \"$_pulse_cmd\"; and \"$_pulse_exe\" terminal hook --kind start --cwd \"$PWD\" --command \"$_pulse_cmd\" --session \"$fish_pid\" 2>/dev/null\n\
             end\n\
             function _pulse_postexec --on-event fish_postexec\n\
             \x20 test -n \"$_pulse_cmd\"; and \"$_pulse_exe\" terminal hook --kind end --cwd \"$PWD\" --command \"$_pulse_cmd\" --session \"$fish_pid\" 2>/dev/null\n\
             \x20 set -g _pulse_cmd ''\n\
             end\n"
        ),
    }
}

/// Writes the hook event. Called by the shell on every command, so it must be
/// fast and never fail loudly: anything unusable is skipped.
pub fn hook(
    store: &Store,
    ended: bool,
    cwd: &Path,
    command: &str,
    session: &str,
) -> io::Result<()> {
    // only the first word, and only when it is a plain word
    let word = command.split_whitespace().next().unwrap_or_default();
    if word.is_empty() || word.len() > 200 || word.chars().any(char::is_control) {
        return Ok(());
    }
    store.append_event(&Event {
        v: PROTOCOL_VERSION,
        t: now(),
        source: TERMINAL_SOURCE.into(),
        session: session.to_owned(),
        project: project::resolve(cwd),
        kind: if ended {
            EventKind::CommandEnded
        } else {
            EventKind::CommandSubmitted
        },
        model: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_shells_from_their_own_signals() {
        assert_eq!(
            detect_from(Some("5.9"), Some("5.2"), None, None),
            Some(Shell::Zsh)
        );
        assert_eq!(
            detect_from(None, Some("5.2"), Some("123"), None),
            Some(Shell::Fish)
        );
        assert_eq!(
            detect_from(None, Some("5.2"), None, None),
            Some(Shell::Bash)
        );
        assert_eq!(
            detect_from(None, None, None, Some("/usr/bin/zsh")),
            Some(Shell::Zsh)
        );
        assert_eq!(detect_from(None, None, None, Some("/bin/sh")), None);
        assert_eq!(detect_from(None, None, None, None), None);
    }

    #[test]
    fn snippets_carry_the_binary_path_the_kinds_and_a_recursion_guard() {
        let exe = Path::new("/opt/pulse/pulse");
        for shell in [Shell::Bash, Shell::Zsh, Shell::Fish] {
            let snippet = snippet(exe, shell);
            assert!(snippet.contains("/opt/pulse/pulse"), "{shell:?}");
            assert!(snippet.contains("--kind start"), "{shell:?}");
            assert!(snippet.contains("--kind end"), "{shell:?}");
            assert!(snippet.contains("--cwd \"$PWD\""), "{shell:?}");
            assert!(snippet.contains("terminal hook"), "{shell:?}");
        }
        assert!(snippet(exe, Shell::Bash).contains("~/.bashrc"));
        assert!(snippet(exe, Shell::Zsh).contains("~/.zshrc"));
        assert!(snippet(exe, Shell::Fish).contains("~/.config/fish/config.fish"));
    }

    #[test]
    fn a_path_with_a_quote_stays_quoted() {
        let exe = Path::new("/opt/o'pulse/pulse");
        assert!(snippet(exe, Shell::Zsh).contains(r"'/opt/o'\''pulse/pulse'"));
    }

    #[test]
    fn hooks_store_only_the_first_word_and_resolve_the_project() {
        let root = tempfile::tempdir().unwrap();
        fs_dir(&root);
        let store = Store::new(root.path().to_path_buf());

        hook(
            &store,
            false,
            &root.path().join("src"),
            "git commit -m 'wip'",
            "123",
        )
        .unwrap();
        hook(
            &store,
            true,
            &root.path().join("src"),
            "npm test -- --watch",
            "123",
        )
        .unwrap();
        hook(
            &store,
            false,
            &root.path().join("src"),
            "line\nbreak",
            "123",
        )
        .unwrap();
        hook(&store, false, &root.path().join("src"), "", "123").unwrap();

        let events = store.events(crate::now() - 10, crate::now() + 10);
        // the newline after "line" is dropped with the rest, the word stays;
        // only the empty command is skipped
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, EventKind::CommandSubmitted);
        assert_eq!(events[1].kind, EventKind::CommandEnded);
        assert_eq!(events[2].kind, EventKind::CommandSubmitted);
        assert_eq!(events[0].session, "123");
        assert!(
            events[0].project.starts_with("local/"),
            "{}",
            events[0].project
        );
        let text = serde_json::to_string(&events[0]).unwrap();
        assert!(
            !text.contains("commit") && !text.contains("npm"),
            "arguments must not be stored"
        );
    }

    fn fs_dir(root: &tempfile::TempDir) {
        std::fs::create_dir_all(root.path().join("src")).unwrap();
    }
}
