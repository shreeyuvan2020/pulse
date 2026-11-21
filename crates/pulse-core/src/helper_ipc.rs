//! The wire format between `pulse` and its `pulse-wayland` helper, which
//! does what Wayland only allows through desktop services: asking for a
//! screen to record and reading the idle timer. `pulse` starts the helper and
//! talks to it over the helper's stdin and stdout.
//!
//! - `pulse` to helper: one text command per line, see [`Command`].
//! - helper to `pulse`: binary messages, `tag: u8`, `length: u32` and then
//!   `length` bytes, all little endian, see [`Message`].

use std::io::{self, Read, Write};

/// Larger than any real screen (16K at 4 bytes a pixel is 530 MB, and no
/// compositor hands out that), small enough to refuse garbage.
const MAX_PAYLOAD: usize = 256 << 20;

const TAG_HELLO: u8 = 1;
const TAG_IDLE: u8 = 2;
const TAG_CAPTURE: u8 = 3;
const TAG_FRAME: u8 = 4;

/// What `pulse` asks of the helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Start sharing the screen. The desktop may ask the person to pick one.
    CaptureOn,
    /// Stop sharing, so the desktop's "recording" indicator goes away.
    CaptureOff,
}

impl Command {
    pub fn line(self) -> &'static str {
        match self {
            Command::CaptureOn => "capture on",
            Command::CaptureOff => "capture off",
        }
    }

    pub fn parse(line: &str) -> Option<Command> {
        match line.trim() {
            "capture on" => Some(Command::CaptureOn),
            "capture off" => Some(Command::CaptureOff),
            _ => None,
        }
    }
}

/// Where screen sharing stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureState {
    Off,
    /// The desktop is asking the person which screen to share.
    Waiting,
    Running,
    /// `retry` is false when the person said no: asking again would only nag.
    Failed {
        retry: bool,
        reason: String,
    },
}

/// One screen image, 4 bytes a pixel in B, G, R, A order, like every capture
/// backend in `pulse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row, at least `width * 4`.
    pub stride: u32,
    pub bgra: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// First thing the helper says: which idle timer it found, or "none".
    Hello {
        idle_backend: String,
    },
    /// Seconds since the last keyboard or mouse input, sent about once a
    /// second. `None` when the desktop gives no way to know.
    Idle {
        seconds: Option<u64>,
    },
    Capture(CaptureState),
    Frame(RawFrame),
}

/// Callers that share a pipe between threads take one lock around the whole
/// call, so messages never interleave.
pub fn write_message(out: &mut impl Write, message: &Message) -> io::Result<()> {
    // `head` is small; a frame's pixels are written straight from `body` so a
    // full screen is never copied just to be framed
    let (tag, head, body): (u8, Vec<u8>, &[u8]) = match message {
        Message::Hello { idle_backend } => (TAG_HELLO, idle_backend.as_bytes().to_vec(), &[]),
        Message::Idle { seconds } => (
            TAG_IDLE,
            seconds.unwrap_or(u64::MAX).to_le_bytes().to_vec(),
            &[],
        ),
        Message::Capture(state) => {
            let (code, text) = match state {
                CaptureState::Off => (0u8, ""),
                CaptureState::Waiting => (1, ""),
                CaptureState::Running => (2, ""),
                CaptureState::Failed {
                    retry: true,
                    reason,
                } => (3, reason.as_str()),
                CaptureState::Failed {
                    retry: false,
                    reason,
                } => (4, reason.as_str()),
            };
            let mut payload = vec![code];
            payload.extend_from_slice(text.as_bytes());
            (TAG_CAPTURE, payload, &[])
        }
        Message::Frame(frame) => {
            let mut head = Vec::with_capacity(12);
            head.extend_from_slice(&frame.width.to_le_bytes());
            head.extend_from_slice(&frame.height.to_le_bytes());
            head.extend_from_slice(&frame.stride.to_le_bytes());
            (TAG_FRAME, head, frame.bgra.as_slice())
        }
    };
    let total = head.len() + body.len();
    let length = u32::try_from(total)
        .ok()
        .filter(|_| total <= MAX_PAYLOAD)
        .ok_or_else(|| io::Error::other("message too large"))?;
    out.write_all(&[tag])?;
    out.write_all(&length.to_le_bytes())?;
    out.write_all(&head)?;
    out.write_all(body)?;
    out.flush()
}

/// The next message, or `None` when the other side closed the pipe between
/// messages.
pub fn read_message(input: &mut impl Read) -> io::Result<Option<Message>> {
    let mut header = [0u8; 5];
    // EOF before the first byte is a clean close; EOF inside a header is not
    loop {
        match input.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    input.read_exact(&mut header[1..])?;
    let length = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if length > MAX_PAYLOAD {
        return Err(bad("message too large"));
    }
    let mut payload = vec![0u8; length];
    input.read_exact(&mut payload)?;
    let message = match header[0] {
        TAG_HELLO => Message::Hello {
            idle_backend: String::from_utf8_lossy(&payload).into_owned(),
        },
        TAG_IDLE => {
            let bytes: [u8; 8] = payload
                .as_slice()
                .try_into()
                .map_err(|_| bad("idle message has the wrong size"))?;
            let seconds = u64::from_le_bytes(bytes);
            Message::Idle {
                seconds: (seconds != u64::MAX).then_some(seconds),
            }
        }
        TAG_CAPTURE => {
            let (&code, text) = payload
                .split_first()
                .ok_or_else(|| bad("empty capture message"))?;
            let reason = String::from_utf8_lossy(text).into_owned();
            Message::Capture(match code {
                0 => CaptureState::Off,
                1 => CaptureState::Waiting,
                2 => CaptureState::Running,
                3 => CaptureState::Failed {
                    retry: true,
                    reason,
                },
                4 => CaptureState::Failed {
                    retry: false,
                    reason,
                },
                _ => return Err(bad("unknown capture state")),
            })
        }
        TAG_FRAME => {
            if payload.len() < 12 {
                return Err(bad("frame message is too short"));
            }
            let field = |at: usize| {
                u32::from_le_bytes([
                    payload[at],
                    payload[at + 1],
                    payload[at + 2],
                    payload[at + 3],
                ])
            };
            let (width, height, stride) = (field(0), field(4), field(8));
            let bgra = payload[12..].to_vec();
            let rows_fit = (stride as usize)
                .checked_mul(height as usize)
                .is_some_and(|needed| needed <= bgra.len());
            let row_fits = (width as usize)
                .checked_mul(4)
                .is_some_and(|row| row <= stride as usize);
            if width == 0 || height == 0 || !rows_fit || !row_fits {
                return Err(bad("frame size does not match its pixels"));
            }
            Message::Frame(RawFrame {
                width,
                height,
                stride,
                bgra,
            })
        }
        _ => return Err(bad("unknown message")),
    };
    Ok(Some(message))
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(message: &Message) -> Message {
        let mut bytes = Vec::new();
        write_message(&mut bytes, message).unwrap();
        read_message(&mut bytes.as_slice()).unwrap().unwrap()
    }

    #[test]
    fn every_message_survives_the_pipe() {
        let frame = RawFrame {
            width: 3,
            height: 2,
            // stride wider than the row, so padding must be kept
            stride: 16,
            bgra: (0..32).collect(),
        };
        for message in [
            Message::Hello {
                idle_backend: "gnome-mutter".into(),
            },
            Message::Idle { seconds: Some(0) },
            Message::Idle { seconds: Some(125) },
            Message::Idle { seconds: None },
            Message::Capture(CaptureState::Off),
            Message::Capture(CaptureState::Waiting),
            Message::Capture(CaptureState::Running),
            Message::Capture(CaptureState::Failed {
                retry: true,
                reason: "portal went away".into(),
            }),
            Message::Capture(CaptureState::Failed {
                retry: false,
                reason: "you said no".into(),
            }),
            Message::Frame(frame),
        ] {
            assert_eq!(roundtrip(&message), message);
        }
    }

    #[test]
    fn messages_are_read_one_at_a_time_and_a_closed_pipe_is_not_an_error() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &Message::Idle { seconds: Some(1) }).unwrap();
        write_message(&mut bytes, &Message::Idle { seconds: Some(2) }).unwrap();
        let mut input = bytes.as_slice();
        assert_eq!(
            read_message(&mut input).unwrap(),
            Some(Message::Idle { seconds: Some(1) })
        );
        assert_eq!(
            read_message(&mut input).unwrap(),
            Some(Message::Idle { seconds: Some(2) })
        );
        assert_eq!(read_message(&mut input).unwrap(), None);
    }

    #[test]
    fn a_pipe_cut_mid_message_is_an_error() {
        let mut bytes = Vec::new();
        write_message(&mut bytes, &Message::Idle { seconds: Some(7) }).unwrap();
        bytes.pop();
        assert!(read_message(&mut bytes.as_slice()).is_err());
        // and so is a pipe cut inside the header
        assert!(read_message(&mut [2u8, 8].as_slice()).is_err());
    }

    #[test]
    fn frames_that_lie_about_their_size_are_refused() {
        let good = |width: u32, height: u32, stride: u32, len: usize| {
            let mut payload = Vec::new();
            payload.extend_from_slice(&width.to_le_bytes());
            payload.extend_from_slice(&height.to_le_bytes());
            payload.extend_from_slice(&stride.to_le_bytes());
            payload.resize(12 + len, 0);
            let mut bytes = vec![TAG_FRAME];
            bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&payload);
            read_message(&mut bytes.as_slice())
        };
        assert!(good(4, 4, 16, 64).is_ok());
        // one byte short of stride * height
        assert!(good(4, 4, 16, 63).is_err());
        // a row wider than the stride
        assert!(good(5, 4, 16, 64).is_err());
        // the product overflowing must not wrap around to something small
        assert!(good(4, u32::MAX, u32::MAX, 64).is_err());
        assert!(good(0, 4, 16, 64).is_err());
    }

    #[test]
    fn unknown_tags_and_oversized_lengths_are_refused() {
        assert!(read_message(&mut [9u8, 0, 0, 0, 0].as_slice()).is_err());
        let mut huge = vec![TAG_FRAME];
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(read_message(&mut huge.as_slice()).is_err());
    }

    #[test]
    fn commands_parse_their_own_lines() {
        for command in [Command::CaptureOn, Command::CaptureOff] {
            assert_eq!(Command::parse(command.line()), Some(command));
        }
        assert_eq!(Command::parse("  capture on\n"), Some(Command::CaptureOn));
        assert_eq!(Command::parse("capture"), None);
    }
}
