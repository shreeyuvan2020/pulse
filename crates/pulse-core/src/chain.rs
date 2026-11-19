//! The evidence hash chain. Every recorded segment is hashed together with
//! the hash of the segment before it, so segments cannot be reordered,
//! dropped from the middle, or made up later without breaking every link
//! after them. Once links are sent to a server as they are made, a recording
//! also cannot be produced after the fact.

use sha2::{Digest, Sha256};

pub type Hash = [u8; 32];

/// The `previous` of the first segment on a device.
pub const GENESIS: Hash = [0; 32];

pub fn sha256(bytes: &[u8]) -> Hash {
    Sha256::digest(bytes).into()
}

/// One recorded second as hashed: the time, the screen change score and
/// whether the OS saw input during that second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Second {
    pub t: i64,
    pub change: u16,
    pub input: bool,
}

/// `sha256` over 11 bytes per second: `t` (big-endian i64), `change`
/// (big-endian u16), `input` (one byte, 0 or 1).
pub fn timeline_hash(seconds: &[Second]) -> Hash {
    let mut hasher = Sha256::new();
    for second in seconds {
        hasher.update(second.t.to_be_bytes());
        hasher.update(second.change.to_be_bytes());
        hasher.update([u8::from(second.input)]);
    }
    hasher.finalize().into()
}

/// The next link: `sha256("pulse.chain.v1" || previous || start || end || video || timeline)`,
/// with `start` and `end` as big-endian i64 unix seconds.
pub fn link(previous: &Hash, start: i64, end: i64, video: &Hash, timeline: &Hash) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(b"pulse.chain.v1");
    hasher.update(previous);
    hasher.update(start.to_be_bytes());
    hasher.update(end.to_be_bytes());
    hasher.update(video);
    hasher.update(timeline);
    hasher.finalize().into()
}

pub fn to_hex(hash: &Hash) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn from_hex(text: &str) -> Option<Hash> {
    if text.len() != 64 {
        return None;
    }
    let mut hash = [0; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values_stay_fixed() {
        // pinned so a server in any language can check it reproduces the same bytes
        let seconds = [
            Second {
                t: 1_795_051_591,
                change: 12,
                input: true,
            },
            Second {
                t: 1_795_051_592,
                change: 0,
                input: false,
            },
        ];
        let timeline = timeline_hash(&seconds);
        let video = sha256(b"video");
        let first = link(&GENESIS, 1_795_051_591, 1_795_051_593, &video, &timeline);

        assert_eq!(
            to_hex(&timeline),
            to_hex(&sha256(&[
                0, 0, 0, 0, 0x6a, 0xfe, 0x50, 0x47, 0, 12, 1, 0, 0, 0, 0, 0x6a, 0xfe, 0x50, 0x48,
                0, 0, 0,
            ]))
        );
        assert_eq!(from_hex(&to_hex(&first)), Some(first));
        assert_ne!(
            first,
            link(&first, 1_795_051_591, 1_795_051_593, &video, &timeline)
        );
        assert_ne!(
            first,
            link(&GENESIS, 1_795_051_591, 1_795_051_594, &video, &timeline)
        );
        assert_eq!(from_hex("zz"), None);
    }
}
