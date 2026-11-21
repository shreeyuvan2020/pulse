//! Wayland support. A Wayland program cannot look at the screen or ask for
//! the idle time on its own, so Pulse starts its helper, `pulse-wayland`
//! (installed next to `pulse`), which asks the desktop's own services: the
//! ScreenCast portal for the screen and the compositor for input idle time.
//! This module owns that helper process and turns its messages into the same
//! two answers the other platforms give: seconds since last input, and the
//! latest screen image.
//!
//! Nothing here blocks the daemon's one-second loop. A desktop that is still
//! asking the person which screen to share just means no image yet.

use std::env;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use pulse_core::helper_ipc::{
    CaptureState, Command as HelperCommand, Message, RawFrame, read_message,
};

use crate::paths;
use crate::recorder::encode::Frame;

/// A helper that stopped is not restarted more often than this.
const RESTART_AFTER: Duration = Duration::from_secs(30);
/// A screen image or idle reading older than this is not trusted.
const FRESH_FOR: Duration = Duration::from_secs(3);
/// After a screen sharing failure that may pass, wait this long before asking
/// the desktop again, so a broken setup does not pop up dialogs all day.
const RETRY_CAPTURE_AFTER: Duration = Duration::from_secs(300);
/// Do not repeat a "start sharing" request while the first is being answered.
const REASK_AFTER: Duration = Duration::from_secs(3);

/// Whether this is a Wayland login. An X11 session that also has a Wayland
/// socket in its environment (a nested compositor) still counts as X11.
pub fn is_session() -> bool {
    match env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("wayland") => true,
        Ok("x11") => false,
        _ => env::var_os("WAYLAND_DISPLAY").is_some(),
    }
}

/// The running helper, started on first use.
pub fn client() -> Result<Arc<Client>, String> {
    static CLIENT: Mutex<Option<(Arc<Client>, Instant)>> = Mutex::new(None);
    let mut slot = CLIENT.lock().expect("client lock");
    if let Some((client, started)) = &*slot {
        if client.alive() {
            return Ok(Arc::clone(client));
        }
        if started.elapsed() < RESTART_AFTER {
            return Err("the Wayland helper stopped".into());
        }
    }
    let client = Arc::new(Client::start()?);
    *slot = Some((Arc::clone(&client), Instant::now()));
    Ok(client)
}

/// Seconds since the last input, from the desktop's own idle timer.
pub fn idle_seconds() -> Option<u64> {
    client().ok()?.idle_seconds()
}

fn helper_path() -> PathBuf {
    if let Some(path) = env::var_os("PULSE_WAYLAND_HELPER") {
        return path.into();
    }
    if let Ok(exe) = env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let beside = dir.join("pulse-wayland");
        if beside.is_file() {
            return beside;
        }
    }
    // any other place it is installed, found through PATH
    PathBuf::from("pulse-wayland")
}

pub struct Client {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

struct State {
    alive: bool,
    /// Which idle timer the helper found, once it has said.
    backend: Option<String>,
    idle: Option<(Option<u64>, Instant)>,
    capture: CaptureState,
    frame: Option<(RawFrame, Instant)>,
    /// Whether the recorder has been asking for images since the last release.
    wants_capture: bool,
    last_asked: Option<Instant>,
    failed_at: Option<Instant>,
}

impl Client {
    fn start() -> Result<Client, String> {
        let path = helper_path();
        let mut child = Command::new(&path)
            .arg("--data-dir")
            .arg(paths::data_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    "this is a Wayland session, and Pulse needs its Wayland helper, `pulse-wayland`, installed next to `pulse` (the release packages include it)".to_owned()
                } else {
                    format!("cannot start {}: {error}", path.display())
                }
            })?;
        let stdin = child.stdin.take().ok_or("the helper has no stdin")?;
        let stdout = child.stdout.take().ok_or("the helper has no stdout")?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                alive: true,
                backend: None,
                idle: None,
                capture: CaptureState::Off,
                frame: None,
                wants_capture: false,
                last_asked: None,
                failed_at: None,
            }),
            changed: Condvar::new(),
        });
        let reader = Arc::clone(&shared);
        thread::spawn(move || read_helper(BufReader::new(stdout), &reader));
        Ok(Client {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            shared,
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.shared.state.lock().expect("wayland state lock")
    }

    fn alive(&self) -> bool {
        self.state().alive
    }

    /// Which idle timer the desktop gave, for `pulse status`.
    pub fn idle_backend(&self) -> Option<String> {
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(self.state(), Duration::from_secs(3), |state| {
                state.backend.is_none() && state.alive
            })
            .expect("wayland state lock");
        state.backend.clone()
    }

    /// The latest idle reading. The first call waits a moment for the helper
    /// to start up and say what it found.
    pub fn idle_seconds(&self) -> Option<u64> {
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(self.state(), Duration::from_secs(3), |state| {
                state.idle.is_none() && state.alive
            })
            .expect("wayland state lock");
        match state.idle {
            Some((seconds, at)) if at.elapsed() < FRESH_FOR => seconds,
            _ => None,
        }
    }

    /// The latest screen image. The first call starts screen sharing, which
    /// may take a few seconds, or until the person answers the desktop's
    /// question, so it returns what it has and never waits.
    pub fn grab(&self) -> Option<Frame> {
        let mut state = self.state();
        state.wants_capture = true;
        let retry_ok = match &state.capture {
            CaptureState::Off => true,
            CaptureState::Failed { retry: true, .. } => state
                .failed_at
                .is_none_or(|at| at.elapsed() >= RETRY_CAPTURE_AFTER),
            _ => false,
        };
        let asked_recently = state
            .last_asked
            .is_some_and(|at| at.elapsed() < REASK_AFTER);
        if retry_ok && !asked_recently {
            state.last_asked = Some(Instant::now());
            drop(state);
            self.send(HelperCommand::CaptureOn);
            state = self.state();
        }
        let (frame, at) = state.frame.as_ref()?;
        (at.elapsed() < FRESH_FOR).then(|| Frame {
            width: frame.width as usize,
            height: frame.height as usize,
            stride: frame.stride as usize,
            bgra: frame.bgra.clone(),
        })
    }

    /// Stops screen sharing, so the desktop's recording indicator goes off
    /// while nothing is being recorded.
    pub fn release(&self) {
        {
            let mut state = self.state();
            state.wants_capture = false;
            state.frame = None;
            state.last_asked = None;
        }
        self.send(HelperCommand::CaptureOff);
    }

    fn send(&self, command: HelperCommand) {
        let mut stdin = self.stdin.lock().expect("helper stdin lock");
        // a dead helper is noticed by the reader thread, which clears `alive`
        let _ = writeln!(stdin, "{}", command.line()).and_then(|()| stdin.flush());
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut child = self.child.lock().expect("helper child lock");
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn read_helper(mut stdout: BufReader<std::process::ChildStdout>, shared: &Shared) {
    loop {
        match read_message(&mut stdout) {
            Ok(Some(message)) => apply(message, shared),
            Ok(None) => break,
            Err(error) => {
                eprintln!("pulse: the Wayland helper sent something unreadable: {error}");
                break;
            }
        }
    }
    shared.state.lock().expect("wayland state lock").alive = false;
    shared.changed.notify_all();
}

fn apply(message: Message, shared: &Shared) {
    let mut state = shared.state.lock().expect("wayland state lock");
    match message {
        Message::Hello { idle_backend } => state.backend = Some(idle_backend),
        Message::Idle { seconds } => state.idle = Some((seconds, Instant::now())),
        Message::Frame(frame) => state.frame = Some((frame, Instant::now())),
        Message::Capture(capture) => {
            if capture != state.capture {
                announce(&capture);
            }
            if matches!(capture, CaptureState::Failed { .. }) {
                state.failed_at = Some(Instant::now());
            }
            if !matches!(capture, CaptureState::Running) {
                // an image from a stream that has stopped must not pass for current
                state.frame = None;
            }
            state.capture = capture;
        }
    }
    drop(state);
    shared.changed.notify_all();
}

/// Tells the person what screen sharing is doing, but not on every ordinary
/// stop and start.
fn announce(capture: &CaptureState) {
    static SAID_WAITING: AtomicBool = AtomicBool::new(false);
    match capture {
        CaptureState::Waiting => {
            if !SAID_WAITING.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "pulse: waiting for you to choose which screen Pulse may record, in the window your desktop opened. Pulse keeps no recording until you do, and the desktop remembers your choice"
                );
            }
        }
        CaptureState::Failed { retry, reason } => {
            if *retry {
                eprintln!(
                    "pulse: cannot record the screen: {reason}. Trying again in a few minutes"
                );
            } else {
                eprintln!(
                    "pulse: cannot record the screen: {reason}. Pulse will not ask again until it is restarted"
                );
            }
        }
        CaptureState::Off | CaptureState::Running => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared {
            state: Mutex::new(State {
                alive: true,
                backend: None,
                idle: None,
                capture: CaptureState::Off,
                frame: None,
                wants_capture: false,
                last_asked: None,
                failed_at: None,
            }),
            changed: Condvar::new(),
        }
    }

    fn frame() -> RawFrame {
        RawFrame {
            width: 1,
            height: 1,
            stride: 4,
            bgra: vec![1, 2, 3, 4],
        }
    }

    #[test]
    fn the_last_image_of_a_stopped_stream_is_not_kept() {
        let shared = shared();
        apply(Message::Capture(CaptureState::Running), &shared);
        apply(Message::Frame(frame()), &shared);
        assert!(shared.state.lock().unwrap().frame.is_some());
        // the person closed the share, or the stream broke: that image is stale
        apply(Message::Capture(CaptureState::Off), &shared);
        assert!(shared.state.lock().unwrap().frame.is_none());
    }

    #[test]
    fn a_failed_share_remembers_when_so_asking_again_can_wait() {
        let shared = shared();
        apply(
            Message::Capture(CaptureState::Failed {
                retry: true,
                reason: "portal went away".into(),
            }),
            &shared,
        );
        assert!(shared.state.lock().unwrap().failed_at.is_some());
    }
}
