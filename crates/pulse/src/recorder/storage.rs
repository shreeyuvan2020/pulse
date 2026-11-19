//! Keeps recordings from filling the disk. Once the recordings folder is over
//! its limit, the oldest segment videos are deleted. A deleted segment keeps
//! its line in the hash chain and its timeline file (a few KB), so the chain
//! stays unbroken and `verify` still passes; only the video itself is gone, so
//! that stretch can no longer be submitted. Recording pauses when the disk is
//! nearly full instead of filling it.

use std::env;
use std::fs;
use std::io;
use std::path::Path;

use super::read_chain;

/// Pulse does not start a new segment with less free space than this: 1 GiB.
pub const MIN_FREE_BYTES: u64 = 1_073_741_824;

/// The limit on recording size: `PULSE_MAX_STORAGE_GB`, or 5 GB.
pub fn cap_bytes() -> u64 {
    let gigabytes = env::var("PULSE_MAX_STORAGE_GB")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(5.0);
    (gigabytes * 1e9) as u64
}

pub fn free_bytes(dir: &Path) -> Option<u64> {
    fs4::available_space(dir).ok()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub bytes: u64,
    pub segments: usize,
    /// Segments whose video was deleted to save space.
    pub pruned: usize,
}

pub fn usage(dir: &Path) -> Usage {
    let mut usage = Usage::default();
    for link in read_chain(dir) {
        usage.segments += 1;
        match fs::metadata(dir.join(&link.file)) {
            Ok(meta) => usage.bytes += meta.len(),
            Err(_) => usage.pruned += 1,
        }
    }
    usage
}

/// Deletes the oldest segment videos until the rest fit in `cap` bytes.
/// Returns how many videos and bytes were deleted.
pub fn prune(dir: &Path, cap: u64) -> io::Result<(usize, u64)> {
    let videos: Vec<(String, u64)> = read_chain(dir)
        .into_iter()
        .filter_map(|link| {
            let size = fs::metadata(dir.join(&link.file)).ok()?.len();
            Some((link.file, size))
        })
        .collect();
    let mut total: u64 = videos.iter().map(|(_, size)| size).sum();
    let (mut count, mut freed) = (0, 0);
    for (file, size) in videos {
        if total <= cap {
            break;
        }
        fs::remove_file(dir.join(file))?;
        total -= size;
        freed += size;
        count += 1;
    }
    Ok((count, freed))
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1e9)
    } else {
        format!("{:.0} MB", bytes as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::{Link, Timeline, timeline_file, verify};
    use pulse_core::chain::{self, GENESIS, Hash, Second};
    use std::io::Write;

    /// Writes a fake segment of `size` bytes and its chain line, the way the recorder does.
    fn add_segment(dir: &Path, previous: &mut Hash, start: i64, size: usize) {
        let file = format!("{start}.mp4");
        let bytes = vec![(start % 256) as u8; size];
        fs::write(dir.join(&file), &bytes).unwrap();
        let seconds = vec![Second {
            t: start,
            change: 3,
            input: true,
        }];
        fs::write(
            dir.join(timeline_file(&file)),
            serde_json::to_vec(&Timeline {
                seconds: vec![(start, 3, 1)],
            })
            .unwrap(),
        )
        .unwrap();
        let video = chain::sha256(&bytes);
        let timeline = chain::timeline_hash(&seconds);
        let link = chain::link(previous, start, start + 1, &video, &timeline);
        let entry = Link {
            start,
            end: start + 1,
            file,
            video: chain::to_hex(&video),
            timeline: chain::to_hex(&timeline),
            previous: chain::to_hex(previous),
            link: chain::to_hex(&link),
            projects: Default::default(),
        };
        let mut chain_file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("chain.jsonl"))
            .unwrap();
        writeln!(chain_file, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        *previous = link;
    }

    #[test]
    fn prunes_oldest_videos_first_and_the_chain_still_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let mut previous = GENESIS;
        for (start, size) in [(100, 400), (200, 300), (300, 200), (400, 100)] {
            add_segment(dir.path(), &mut previous, start, size);
        }
        assert_eq!(usage(dir.path()).bytes, 1000);

        // 1000 bytes over a cap of 550: the 400 and 300 byte videos go, leaving 300
        assert_eq!(prune(dir.path(), 550).unwrap(), (2, 700));
        assert!(!dir.path().join("100.mp4").exists());
        assert!(!dir.path().join("200.mp4").exists());
        assert!(dir.path().join("300.mp4").exists());
        assert_eq!(
            usage(dir.path()),
            Usage {
                bytes: 300,
                segments: 4,
                pruned: 2
            }
        );
        assert_eq!(prune(dir.path(), 550).unwrap(), (0, 0));

        let verified = verify(dir.path()).unwrap();
        assert_eq!((verified.links.len(), verified.pruned), (4, 2));
    }

    #[test]
    fn verify_still_catches_edits_next_to_pruned_segments() {
        let dir = tempfile::tempdir().unwrap();
        let mut previous = GENESIS;
        for start in [100, 200, 300] {
            add_segment(dir.path(), &mut previous, start, 50);
        }
        prune(dir.path(), 60).unwrap();

        // a kept video that was changed
        fs::write(dir.path().join("300.mp4"), vec![0; 50]).unwrap();
        assert!(
            verify(dir.path())
                .unwrap_err()
                .contains("changed after recording")
        );
        fs::write(dir.path().join("300.mp4"), vec![(300 % 256) as u8; 50]).unwrap();
        assert!(verify(dir.path()).is_ok());

        // a timeline that was changed on a pruned segment
        fs::write(
            dir.path().join(timeline_file("100.mp4")),
            serde_json::to_vec(&Timeline {
                seconds: vec![(100, 999, 1)],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            verify(dir.path())
                .unwrap_err()
                .contains("timeline was changed")
        );
    }
}
