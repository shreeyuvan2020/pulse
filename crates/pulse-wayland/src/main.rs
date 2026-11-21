//! `pulse-wayland`: what the `pulse` tracker cannot do itself on Wayland.
//! It records the screen through the desktop's ScreenCast portal and reads
//! the idle timer, and `pulse` starts it and talks to it over stdin and
//! stdout (see `pulse_core::helper_ipc`). It makes no network connections and
//! writes only one file: the portal's "remember this screen" token.

#[cfg(target_os = "linux")]
mod idle;
#[cfg(target_os = "linux")]
mod screen;

#[cfg(target_os = "linux")]
fn main() {
    linux::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pulse-wayland only exists for Linux");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
use linux::Out;

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{self, BufRead, Write};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use pulse_core::helper_ipc::{Command, Message, write_message};

    use crate::idle::Idle;
    use crate::screen::Capture;

    /// Where messages to `pulse` go. Cloneable, and one message at a time.
    #[derive(Clone)]
    pub struct Out(Arc<Mutex<io::Stdout>>);

    impl Out {
        pub fn send(&self, message: &Message) {
            let mut stdout = self.0.lock().expect("stdout lock");
            if write_message(&mut *stdout, message).is_err() {
                // `pulse` is gone, so there is nobody left to serve
                std::process::exit(0);
            }
        }
    }

    pub fn run() {
        let mut data_dir = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--data-dir" => data_dir = args.next().map(PathBuf::from),
                "--version" => {
                    println!("pulse-wayland {}", env!("CARGO_PKG_VERSION"));
                    return;
                }
                other => {
                    eprintln!("pulse-wayland: unknown argument {other}. It is started by `pulse`.");
                    std::process::exit(2);
                }
            }
        }
        let Some(data_dir) = data_dir else {
            eprintln!("pulse-wayland: it is started by `pulse`, which passes --data-dir");
            std::process::exit(2);
        };

        let out = Out(Arc::new(Mutex::new(io::stdout())));
        let mut idle = Idle::open();
        out.send(&Message::Hello {
            idle_backend: idle.backend().to_owned(),
        });
        {
            let out = out.clone();
            thread::spawn(move || {
                loop {
                    out.send(&Message::Idle {
                        seconds: idle.seconds(),
                    });
                    thread::sleep(Duration::from_secs(1));
                }
            });
        }

        let mut capture = Capture::new(data_dir, out);
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            match Command::parse(&line) {
                Some(Command::CaptureOn) => capture.on(),
                Some(Command::CaptureOff) => capture.off(),
                None => eprintln!("pulse-wayland: ignoring unknown command {line:?}"),
            }
        }
        // `pulse` closed our stdin: it exited. The desktop ends our screen
        // share by itself when this process's bus connection goes away.
        let _ = io::stdout().flush();
    }
}
