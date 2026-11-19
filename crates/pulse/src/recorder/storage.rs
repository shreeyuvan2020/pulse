//! Keeps recordings from filling the disk. Once the recordings folder is over
//! its limit, the oldest segment videos are deleted. A deleted segment keeps
//! its line in the hash chain and its timeline file (a few KB), so the chain
//! stays unbroken and `verify` still passes; only the video itself is gone, so
//! that stretch can no longer be submitted. Recording pauses when the disk is
//! nearly full instead of filling it.

use std::collections::BTreeSet;
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

/// Projects whose footage is never deleted to make room: the ones linked to
/// a program, until their hours are submitted. Kept in `protected.json` next
/// to the recordings. A segment is protected if any project in it is.
pub fn protected(dir: &Path) -> BTreeSet<String> {
    fs::read_to_string(dir.join("protected.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Protects or unprotects a project. Returns whether anything changed.
pub fn set_protected(dir: &Path, project: &str, on: bool) -> io::Result<bool> {
    let mut projects = protected(dir);
    let changed = if on {
        projects.insert(project.to_owned())
    } else {
        projects.remove(project)
    };
    if changed {
        fs::create_dir_all(dir)?;
        fs::write(dir.join("protected.json"), serde_json::to_vec(&projects)?)?;
    }
    Ok(changed)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub bytes: u64,
    pub segments: usize,
    /// Segments whose video was deleted to save space.
    pub pruned: usize,
    /// Bytes of video that will not be deleted because the project is protected.
    pub protected_bytes: u64,
}

pub fn usage(dir: &Path) -> Usage {
    let protected = protected(dir);
    let mut usage = Usage::default();
    for link in read_chain(dir) {
        usage.segments += 1;
        match fs::metadata(dir.join(&link.file)) {
            Ok(meta) => {
                usage.bytes += meta.len();
                if !link.projects.is_disjoint(&protected) {
                    usage.protected_bytes += meta.len();
                }
            }
            Err(_) => usage.pruned += 1,
        }
    }
    usage
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub count: usize,
    pub freed: u64,
    /// Still over the limit, because everything left is protected.
    pub stuck: bool,
}

/// Deletes the oldest unprotected segment videos until the rest fit in `cap`
/// bytes. Protected footage is never deleted, so the folder can stay over
/// the limit; `stuck` says so.
pub fn prune(dir: &Path, cap: u64, protected: &BTreeSet<String>) -> io::Result<Pruned> {
    let videos: Vec<(String, u64, bool)> = read_chain(dir)
        .into_iter()
        .filter_map(|link| {
            let size = fs::metadata(dir.join(&link.file)).ok()?.len();
            Some((link.file, size, !link.projects.is_disjoint(protected)))
        })
        .collect();
    let mut total: u64 = videos.iter().map(|(_, size, _)| size).sum();
    let mut result = Pruned::default();
    for (file, size, keep) in videos {
        if total <= cap {
            break;
        }
        if keep {
            continue;
        }
        fs::remove_file(dir.join(file))?;
        total -= size;
        result.freed += size;
        result.count += 1;
    }
    result.stuck = total > cap;
    Ok(result)
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
        add_segment_for(dir, previous, start, size, &[]);
    }

    fn add_segment_for(
        dir: &Path,
        previous: &mut Hash,
        start: i64,
        size: usize,
        projects: &[&str],
    ) {
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
            projects: projects.iter().map(|project| project.to_string()).collect(),
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
        let none = BTreeSet::new();
        let pruned = prune(dir.path(), 550, &none).unwrap();
        assert_eq!((pruned.count, pruned.freed, pruned.stuck), (2, 700, false));
        assert!(!dir.path().join("100.mp4").exists());
        assert!(!dir.path().join("200.mp4").exists());
        assert!(dir.path().join("300.mp4").exists());
        assert_eq!(
            usage(dir.path()),
            Usage {
                bytes: 300,
                segments: 4,
                pruned: 2,
                protected_bytes: 0
            }
        );
        assert_eq!(prune(dir.path(), 550, &none).unwrap(), Pruned::default());

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
        prune(dir.path(), 60, &BTreeSet::new()).unwrap();

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

    #[test]
    fn protected_projects_are_skipped_even_when_that_leaves_the_folder_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut previous = GENESIS;
        add_segment_for(dir.path(), &mut previous, 100, 400, &["school"]);
        add_segment_for(dir.path(), &mut previous, 200, 300, &["hobby"]);
        add_segment_for(dir.path(), &mut previous, 300, 200, &["hobby", "school"]);
        add_segment_for(dir.path(), &mut previous, 400, 100, &["hobby"]);

        assert!(set_protected(dir.path(), "school", true).unwrap());
        assert!(!set_protected(dir.path(), "school", true).unwrap());
        let protected = protected(dir.path());
        assert_eq!(usage(dir.path()).protected_bytes, 600);

        // cap 500: the oldest unprotected video (300 bytes) goes, then the 100 byte one;
        // the 400 and 200 byte videos stay because "school" is in them, so 600 is left
        let pruned = prune(dir.path(), 500, &protected).unwrap();
        assert_eq!((pruned.count, pruned.freed, pruned.stuck), (2, 400, true));
        assert!(dir.path().join("100.mp4").exists());
        assert!(!dir.path().join("200.mp4").exists());
        assert!(dir.path().join("300.mp4").exists());
        assert!(!dir.path().join("400.mp4").exists());
        assert_eq!(verify(dir.path()).unwrap().pruned, 2);

        // once unprotected, the oldest goes too
        assert!(set_protected(dir.path(), "school", false).unwrap());
        let pruned = prune(dir.path(), 500, &self::protected(dir.path())).unwrap();
        assert_eq!((pruned.count, pruned.stuck), (1, false));
    }
}
