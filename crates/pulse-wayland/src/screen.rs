//! Screen capture on Wayland, the way the desktop wants it: ask the
//! ScreenCast portal (the person picks a screen once, and the desktop shows
//! its "sharing" indicator while we hold it), then read the frames from the
//! PipeWire stream the portal opens for us.
//!
//! Frames come in at whatever rate the compositor likes, so we keep only the
//! latest one (copied at most twice a second) and send it to `pulse` once a
//! second. The stream is closed whenever `pulse` stops asking for it, so the
//! indicator is only on while Pulse is actually recording.

use std::fs;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::Session;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use pipewire as pw;
use pulse_core::helper_ipc::{CaptureState, Message, RawFrame};
use pw::{properties::properties, spa};

use crate::Out;

/// How often a new image is copied out of the stream.
const COPY_EVERY: Duration = Duration::from_millis(400);
/// A stream that never delivers an image in this long is given up on.
const FIRST_FRAME_WITHIN: Duration = Duration::from_secs(10);
/// The portal asking the person something takes longer than this.
const SHOW_WAITING_AFTER: Duration = Duration::from_secs(2);

type Latest = Arc<Mutex<Option<RawFrame>>>;

/// Starts and stops the screen stream on `pulse`'s request.
pub struct Capture {
    data_dir: PathBuf,
    out: Out,
    worker: Option<Worker>,
}

struct Worker {
    want: Arc<AtomicBool>,
    /// Ends the PipeWire loop once there is one.
    quit: Arc<Mutex<Option<pw::channel::Sender<()>>>>,
    join: JoinHandle<()>,
}

impl Worker {
    fn stop(&self) {
        self.want.store(false, Ordering::SeqCst);
        if let Some(quit) = &*self.quit.lock().expect("quit lock") {
            let _ = quit.send(());
        }
    }
}

impl Capture {
    pub fn new(data_dir: PathBuf, out: Out) -> Capture {
        Capture {
            data_dir,
            out,
            worker: None,
        }
    }

    pub fn on(&mut self) {
        if let Some(worker) = &self.worker
            && !worker.join.is_finished()
        {
            // still winding down from an earlier stop, or already running
            worker.want.store(true, Ordering::SeqCst);
            return;
        }
        let want = Arc::new(AtomicBool::new(true));
        let quit = Arc::new(Mutex::new(None));
        let join = {
            let (want, quit) = (Arc::clone(&want), Arc::clone(&quit));
            let (data_dir, out) = (self.data_dir.clone(), self.out.clone());
            thread::spawn(move || run(&want, &quit, &data_dir, &out))
        };
        self.worker = Some(Worker { want, quit, join });
    }

    pub fn off(&mut self) {
        if let Some(worker) = &self.worker {
            worker.stop();
        }
    }
}

fn run(
    want: &Arc<AtomicBool>,
    quit: &Arc<Mutex<Option<pw::channel::Sender<()>>>>,
    data_dir: &Path,
    out: &Out,
) {
    // a testing aid: read this node of the user's own PipeWire instead of
    // asking the desktop's portal
    let test_node = std::env::var("PULSE_WAYLAND_PIPEWIRE_NODE")
        .ok()
        .and_then(|node| node.parse::<u32>().ok());
    let (session, node, fd) = match test_node {
        Some(node) => (None, node, None),
        None => match async_io::block_on(open_portal(data_dir, out)) {
            Ok(Portal { session, node, fd }) => (Some(session), node, Some(fd)),
            Err(error) => {
                let declined = matches!(
                    error,
                    ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)
                );
                out.send(&Message::Capture(CaptureState::Failed {
                    // asking again after a "no" would only nag
                    retry: !declined,
                    reason: if declined {
                        "screen sharing was declined".to_owned()
                    } else {
                        format!("the desktop's screen sharing failed: {error}")
                    },
                }));
                return;
            }
        },
    };
    let result = if want.load(Ordering::SeqCst) {
        stream_frames(node, fd, want, quit, out)
    } else {
        Ok(())
    };
    if let Some(session) = session {
        let _ = async_io::block_on(session.close());
    }
    out.send(&Message::Capture(match result {
        Ok(()) => CaptureState::Off,
        Err(reason) => CaptureState::Failed {
            retry: true,
            reason,
        },
    }));
}

struct Portal {
    session: Session<Screencast>,
    node: u32,
    fd: OwnedFd,
}

async fn open_portal(data_dir: &Path, out: &Out) -> Result<Portal, ashpd::Error> {
    let proxy = Screencast::new().await?;
    // the pointer is part of what a reviewer needs to see
    let cursor = match proxy.available_cursor_modes().await {
        Ok(modes) if modes.contains(CursorMode::EMBEDDED) => CursorMode::EMBEDDED,
        _ => CursorMode::HIDDEN,
    };
    let session = proxy.create_session(Default::default()).await?;
    let token_file = data_dir.join("wayland-restore-token");
    let token = fs::read_to_string(&token_file)
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty());
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(SourceType::MONITOR)
                .set_multiple(false)
                // the desktop remembers the pick, so it asks only once
                .set_restore_token(token.as_deref())
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await?;

    // a restored pick answers at once; anything slower means a person is deciding
    let answered = Arc::new(AtomicBool::new(false));
    {
        let (answered, out) = (Arc::clone(&answered), out.clone());
        thread::spawn(move || {
            thread::sleep(SHOW_WAITING_AFTER);
            if !answered.load(Ordering::SeqCst) {
                out.send(&Message::Capture(CaptureState::Waiting));
            }
        });
    }
    let started = proxy.start(&session, None, Default::default()).await;
    answered.store(true, Ordering::SeqCst);
    let streams = started?.response()?;
    if let Some(token) = streams.restore_token() {
        save_token(&token_file, token);
    }
    let node = streams
        .streams()
        .first()
        .ok_or(ashpd::Error::NoResponse)?
        .pipe_wire_node_id();
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    Ok(Portal { session, node, fd })
}

fn save_token(path: &Path, token: &str) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let written = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(token.as_bytes()));
    if let Err(error) = written {
        eprintln!("pulse-wayland: cannot remember the screen you picked: {error}");
    }
}

struct StreamData {
    format: spa::param::video::VideoInfoRaw,
    latest: Latest,
    last_copy: Option<Instant>,
    /// First problem with the stream, reported once the loop ends.
    problem: Arc<Mutex<Option<String>>>,
}

fn stream_frames(
    node: u32,
    fd: Option<OwnedFd>,
    want: &Arc<AtomicBool>,
    quit_slot: &Arc<Mutex<Option<pw::channel::Sender<()>>>>,
    out: &Out,
) -> Result<(), String> {
    let fail = |error: pw::Error| format!("cannot read the screen stream: {error}");
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(fail)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(fail)?;
    let core = match fd {
        Some(fd) => context.connect_fd_rc(fd, None),
        None => context.connect_rc(None),
    }
    .map_err(fail)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "pulse",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(fail)?;

    let latest: Latest = Arc::new(Mutex::new(None));
    let problem = Arc::new(Mutex::new(None));
    let data = StreamData {
        format: Default::default(),
        latest: Arc::clone(&latest),
        last_copy: None,
        problem: Arc::clone(&problem),
    };
    let quit_loop = {
        let mainloop = mainloop.clone();
        move || mainloop.quit()
    };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed({
            let quit_loop = quit_loop.clone();
            move |_, data, _, new| {
                if let pw::stream::StreamState::Error(message) = new {
                    data.problem
                        .lock()
                        .expect("problem lock")
                        .get_or_insert(format!("the screen stream broke: {message}"));
                    quit_loop();
                }
            }
        })
        .param_changed(|_, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type == spa::param::format::MediaType::Video
                && media_subtype == spa::param::format::MediaSubtype::Raw
            {
                let _ = data.format.parse(param);
            }
        })
        .process(|stream, data| {
            // taking the buffer out and dropping it unread hands it straight back
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            if data
                .last_copy
                .is_some_and(|copied| copied.elapsed() < COPY_EVERY)
            {
                return;
            }
            let Some(plane) = buffer.datas_mut().first_mut() else {
                return;
            };
            let (offset, size, stride) = {
                let chunk = plane.chunk();
                (
                    chunk.offset() as usize,
                    chunk.size() as usize,
                    chunk.stride(),
                )
            };
            let size_info = data.format.size();
            let Some(source) = plane.data() else {
                data.problem.lock().expect("problem lock").get_or_insert(
                    "the desktop sends the screen as GPU buffers, which Pulse cannot read"
                        .to_owned(),
                );
                return;
            };
            if size == 0 {
                return;
            }
            let swap_red_blue = matches!(
                data.format.format(),
                spa::param::video::VideoFormat::RGBx | spa::param::video::VideoFormat::RGBA
            );
            let end = source.len().min(offset.saturating_add(size));
            let stride = usize::try_from(stride).unwrap_or(0);
            if let Some(frame) = copy_frame(
                source.get(offset..end).unwrap_or(&[]),
                stride,
                size_info.width as usize,
                size_info.height as usize,
                swap_red_blue,
            ) {
                *data.latest.lock().expect("latest lock") = Some(frame);
                data.last_copy = Some(Instant::now());
            }
        })
        .register()
        .map_err(fail)?;

    let values = enum_format().map_err(|error| format!("cannot describe the formats: {error}"))?;
    let mut params = [spa::pod::Pod::from_bytes(&values).ok_or("bad format description")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(fail)?;

    let (quit_sender, quit_receiver) = pw::channel::channel::<()>();
    let _attached = quit_receiver.attach(mainloop.loop_(), {
        let quit_loop = quit_loop.clone();
        move |()| quit_loop()
    });
    *quit_slot.lock().expect("quit lock") = Some(quit_sender);
    // a stop that arrived before the loop existed must not be lost
    if !want.load(Ordering::SeqCst)
        && let Some(quit) = &*quit_slot.lock().expect("quit lock")
    {
        let _ = quit.send(());
    }

    out.send(&Message::Capture(CaptureState::Running));
    let done = Arc::new(AtomicBool::new(false));
    let pump = {
        let (done, want, latest, problem, out) = (
            Arc::clone(&done),
            Arc::clone(want),
            Arc::clone(&latest),
            Arc::clone(&problem),
            out.clone(),
        );
        let quit = Arc::clone(quit_slot);
        thread::spawn(move || pump(&done, &want, &latest, &problem, &quit, &out))
    };
    mainloop.run();
    done.store(true, Ordering::SeqCst);
    let _ = pump.join();
    *quit_slot.lock().expect("quit lock") = None;
    let problem = problem.lock().expect("problem lock").take();
    match problem {
        Some(problem) => Err(problem),
        None => Ok(()),
    }
}

/// Once a second: stop when asked, give up on a stream that shows nothing,
/// otherwise send the latest image.
fn pump(
    done: &AtomicBool,
    want: &AtomicBool,
    latest: &Latest,
    problem: &Mutex<Option<String>>,
    quit: &Mutex<Option<pw::channel::Sender<()>>>,
    out: &Out,
) {
    let started = Instant::now();
    let stop = |reason: Option<&str>| {
        if let Some(reason) = reason {
            problem
                .lock()
                .expect("problem lock")
                .get_or_insert(reason.to_owned());
        }
        if let Some(quit) = &*quit.lock().expect("quit lock") {
            let _ = quit.send(());
        }
    };
    while !done.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_secs(1));
        if !want.load(Ordering::SeqCst) {
            stop(None);
            return;
        }
        let frame = latest.lock().expect("latest lock").clone();
        match frame {
            Some(frame) => out.send(&Message::Frame(frame)),
            None if started.elapsed() > FIRST_FRAME_WITHIN => {
                stop(Some("the screen stream sent no image"));
                return;
            }
            None => {}
        }
        if problem.lock().expect("problem lock").is_some() {
            stop(None);
            return;
        }
    }
}

/// The pixel layouts, sizes and rates we accept: any of the four common
/// 32-bit RGB orders, in plain shared memory (no GPU-only buffers), at most a
/// couple of images a second (less work for the compositor, and a second is
/// all Pulse needs).
fn enum_format() -> Result<Vec<u8>, String> {
    use spa::param::video::VideoFormat;
    use spa::pod::{Value, object, property};
    use spa::utils::{Fraction, Rectangle};

    let object = object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1280,
                height: 720
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 0, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
        property!(
            spa::param::format::FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 2, denom: 1 },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &Value::Object(object),
    )
    .map(|(bytes, _)| bytes.into_inner())
    .map_err(|error| format!("{error:?}"))
}

/// One image out of a stream buffer. `source` starts at the first pixel row;
/// rows are `stride` bytes apart (a buffer may pad them, and `0` means
/// unpadded). Returns B, G, R, A rows with no padding, swapping red and blue
/// when the stream uses R, G, B order. `None` when the buffer is too short for
/// the size the stream announced.
fn copy_frame(
    source: &[u8],
    stride: usize,
    width: usize,
    height: usize,
    swap_red_blue: bool,
) -> Option<RawFrame> {
    if width == 0 || height == 0 {
        return None;
    }
    let row = width.checked_mul(4)?;
    let stride = stride.max(row);
    // the last row needs only its own pixels, not the padding after them
    let needed = stride.checked_mul(height - 1)?.checked_add(row)?;
    if source.len() < needed {
        return None;
    }
    let mut bgra = Vec::with_capacity(row.checked_mul(height)?);
    for y in 0..height {
        let line = &source[y * stride..y * stride + row];
        if swap_red_blue {
            for pixel in line.as_chunks::<4>().0 {
                bgra.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
            }
        } else {
            bgra.extend_from_slice(line);
        }
    }
    Some(RawFrame {
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        stride: u32::try_from(row).ok()?,
        bgra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_rows_are_copied_without_their_padding() {
        // 2x2 pixels, rows 12 bytes apart: 8 bytes of pixels then 4 of padding
        let mut source = Vec::new();
        source.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 0xEE, 0xEE, 0xEE, 0xEE]);
        source.extend_from_slice(&[9, 10, 11, 12, 13, 14, 15, 16]);
        let frame = copy_frame(&source, 12, 2, 2, false).unwrap();
        assert_eq!((frame.width, frame.height, frame.stride), (2, 2, 8));
        assert_eq!(
            frame.bgra,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn rgb_order_is_turned_into_bgr_order_and_alpha_is_kept() {
        let source = [10, 20, 30, 40, 50, 60, 70, 80];
        let frame = copy_frame(&source, 0, 2, 1, true).unwrap();
        assert_eq!(frame.bgra, vec![30, 20, 10, 40, 70, 60, 50, 80]);
    }

    #[test]
    fn a_zero_stride_means_rows_are_packed() {
        let source: Vec<u8> = (0..24).collect();
        let frame = copy_frame(&source, 0, 3, 2, false).unwrap();
        assert_eq!(frame.bgra, source);
    }

    #[test]
    fn a_buffer_shorter_than_the_announced_size_gives_no_image() {
        let source = [0u8; 8 * 2 - 1];
        assert!(copy_frame(&source, 0, 2, 2, false).is_none());
        // exactly enough is fine, even though the last row has no padding after it
        assert!(copy_frame(&[0u8; 12 + 8], 12, 2, 2, false).is_some());
        assert!(copy_frame(&[0u8; 12 + 7], 12, 2, 2, false).is_none());
        assert!(copy_frame(&[], 0, 0, 0, false).is_none());
    }
}
