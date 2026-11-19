//! The screen recorder: one frame per second, cut into 5-minute MP4
//! segments. Each finished segment gets a sidecar with its per-second
//! activity timeline and a line in `chain.jsonl` linking it to the segment
//! before it. Recordings never leave the computer unless you submit them.

pub mod capture;
pub mod codec;
pub mod encode;
pub mod storage;

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pulse_core::activity::Thumbnail;
use pulse_core::chain::{self, GENESIS, Hash, Second};
use serde::{Deserialize, Serialize};

use capture::Capture;
use encode::{SegmentWriter, VideoEncoder, downscale, target_size};

/// One line of `chain.jsonl`. Hashes are lowercase hex.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub start: i64,
    /// Exclusive: the second after the last recorded frame.
    pub end: i64,
    pub file: String,
    pub video: String,
    pub timeline: String,
    pub previous: String,
    pub link: String,
    pub projects: BTreeSet<String>,
}

/// The sidecar next to each segment: `[t, change, input]` per recorded second.
#[derive(Serialize, Deserialize)]
struct Timeline {
    seconds: Vec<(i64, u16, u8)>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub frames: u64,
    pub bytes: u64,
    /// Time spent capturing, scaling and encoding, for measuring overhead.
    pub busy: Duration,
}

struct Segment {
    start: i64,
    file: String,
    size: (usize, usize),
    encoder: VideoEncoder,
    writer: Option<SegmentWriter>,
    seconds: Vec<Second>,
    projects: BTreeSet<String>,
}

pub struct Recorder {
    capture: Capture,
    codec: PathBuf,
    dir: PathBuf,
    previous: Hash,
    low_disk: bool,
    warned_protected: bool,
    segment: Option<Segment>,
    last_thumbnail: Option<Thumbnail>,
    pub stats: Stats,
}

impl Recorder {
    /// `Err` says why this system cannot record.
    /// `codec_dir` is where Cisco's OpenH264 is kept (downloaded on first use).
    pub fn open(dir: PathBuf, codec_dir: &Path) -> Result<Recorder, String> {
        let codec = codec::ensure(codec_dir)?;
        let capture = Capture::open()?;
        let previous = read_chain(&dir)
            .last()
            .and_then(|link| chain::from_hex(&link.link))
            .unwrap_or(GENESIS);
        Ok(Recorder {
            capture,
            codec,
            dir,
            previous,
            low_disk: false,
            warned_protected: false,
            segment: None,
            last_thumbnail: None,
            stats: Stats::default(),
        })
    }

    /// Records the frame for second `t`. `input` says whether the OS saw
    /// keyboard or mouse input during that second.
    pub fn record(&mut self, t: i64, input: bool, projects: &BTreeSet<String>) -> io::Result<()> {
        let started = Instant::now();
        if self
            .segment
            .as_ref()
            .is_some_and(|segment| t - segment.start >= 300)
        {
            self.finish()?;
        }
        if self.segment.is_none() && !self.has_room() {
            return Ok(());
        }
        let Some(frame) = self.capture.grab() else {
            return Ok(());
        };
        let size = target_size(frame.width, frame.height);
        if self
            .segment
            .as_ref()
            .is_some_and(|segment| segment.size != size)
        {
            self.finish()?;
        }
        let segment = match &mut self.segment {
            Some(segment) => segment,
            empty => empty.insert(Segment {
                start: t,
                file: format!("{t}.mp4"),
                size,
                // a fresh encoder per segment makes every file start with a keyframe and parameter sets
                encoder: VideoEncoder::new(&self.codec)?,
                writer: None,
                seconds: Vec::new(),
                projects: BTreeSet::new(),
            }),
        };

        let rgb = downscale(&frame, size.0, size.1);
        let millis = segment.seconds.len() as u64 * 1000;
        let encoded =
            segment
                .encoder
                .encode(&rgb, size.0, size.1, millis, segment.writer.is_none())?;
        let change = self
            .last_thumbnail
            .as_ref()
            .map_or(1000, |last| encoded.thumbnail.change_from(last));
        self.last_thumbnail = Some(encoded.thumbnail);

        let writer = match &mut segment.writer {
            Some(writer) => writer,
            empty => {
                let (Some(sps), Some(pps)) = (encoded.sps, encoded.pps) else {
                    return Err(io::Error::other(
                        "the encoder's first frame had no parameter sets",
                    ));
                };
                fs::create_dir_all(&self.dir)?;
                empty.insert(SegmentWriter::create(
                    &self.dir.join(&segment.file),
                    size.0,
                    size.1,
                    sps,
                    pps,
                )?)
            }
        };
        self.stats.bytes += encoded.sample.len() as u64;
        writer.write(encoded.sample, encoded.keyframe)?;
        segment.seconds.push(Second { t, change, input });
        segment.projects.extend(projects.iter().cloned());
        self.stats.frames += 1;
        self.stats.busy += started.elapsed();
        Ok(())
    }

    /// Whether there is enough free disk to start a new segment. Says so once
    /// when recording pauses and once when it resumes.
    fn has_room(&mut self) -> bool {
        let _ = fs::create_dir_all(&self.dir);
        let low = storage::free_bytes(&self.dir).is_some_and(|free| free < storage::MIN_FREE_BYTES);
        if low != self.low_disk {
            self.low_disk = low;
            if low {
                eprintln!(
                    "pulse: less than {} free on this disk, recording paused until there is more room",
                    storage::format_bytes(storage::MIN_FREE_BYTES)
                );
            } else {
                eprintln!("pulse: disk space is back, recording resumed");
            }
        }
        !low
    }

    /// Closes the current segment and adds it to the chain.
    pub fn finish(&mut self) -> io::Result<Option<Link>> {
        let Some(segment) = self.segment.take() else {
            return Ok(None);
        };
        let (Some(writer), Some(last)) = (segment.writer, segment.seconds.last()) else {
            return Ok(None);
        };
        writer.finish()?;
        let video = chain::sha256(&fs::read(self.dir.join(&segment.file))?);
        let timeline = chain::timeline_hash(&segment.seconds);
        let end = last.t + 1;
        let link = chain::link(&self.previous, segment.start, end, &video, &timeline);

        let sidecar = Timeline {
            seconds: segment
                .seconds
                .iter()
                .map(|second| (second.t, second.change, u8::from(second.input)))
                .collect(),
        };
        fs::write(
            self.dir.join(timeline_file(&segment.file)),
            serde_json::to_vec(&sidecar)?,
        )?;
        let entry = Link {
            start: segment.start,
            end,
            file: segment.file,
            video: chain::to_hex(&video),
            timeline: chain::to_hex(&timeline),
            previous: chain::to_hex(&self.previous),
            link: chain::to_hex(&link),
            projects: segment.projects,
        };
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("chain.jsonl"))?
            .write_all(line.as_bytes())?;
        self.previous = link;
        let cap = storage::cap_bytes();
        match storage::prune(&self.dir, cap, &storage::protected(&self.dir)) {
            Ok(pruned) => {
                if pruned.count > 0 {
                    eprintln!(
                        "pulse: recordings are over the {} limit, deleted the {} oldest videos ({})",
                        storage::format_bytes(cap),
                        pruned.count,
                        storage::format_bytes(pruned.freed)
                    );
                }
                if pruned.stuck && !self.warned_protected {
                    eprintln!(
                        "pulse: recordings are over the limit, but what is left is protected footage, which is never deleted. Submit it or run `pulse unprotect <project>` to free space"
                    );
                }
                self.warned_protected = pruned.stuck;
            }
            Err(error) => eprintln!("pulse: cannot delete old recordings: {error}"),
        }
        Ok(Some(entry))
    }
}

fn timeline_file(video_file: &str) -> String {
    format!("{}.timeline.json", video_file.trim_end_matches(".mp4"))
}

fn read_chain(dir: &Path) -> Vec<Link> {
    fs::File::open(dir.join("chain.jsonl"))
        .map(|file| {
            BufReader::new(file)
                .lines()
                .map_while(Result::ok)
                .filter_map(|line| serde_json::from_str(&line).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug)]
pub struct Verified {
    pub links: Vec<Link>,
    /// Segments whose video was deleted to save space. Their timelines and
    /// links still verify; only the video can no longer be checked or submitted.
    pub pruned: usize,
}

/// Checks every segment against the chain: each link follows the one before
/// it, and the video and timeline files still hash to what was recorded.
/// Returns the verified links, or the first problem found.
pub fn verify(dir: &Path) -> Result<Verified, String> {
    let links = read_chain(dir);
    let mut previous = GENESIS;
    let mut pruned = 0;
    for entry in &links {
        let fail = |problem: &str| Err(format!("segment {}: {problem}", entry.file));
        if chain::from_hex(&entry.previous) != Some(previous) {
            return fail("does not follow the segment before it");
        }
        let Some(video) = chain::from_hex(&entry.video) else {
            return fail("video hash is unreadable");
        };
        match fs::read(dir.join(&entry.file)) {
            Ok(bytes) if chain::sha256(&bytes) != video => {
                return fail("video was changed after recording");
            }
            Ok(_) => {}
            Err(_) => pruned += 1,
        }
        let Some(sidecar) = fs::read(dir.join(timeline_file(&entry.file)))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Timeline>(&bytes).ok())
        else {
            return fail("timeline file is missing or unreadable");
        };
        let seconds: Vec<Second> = sidecar
            .seconds
            .iter()
            .map(|(t, change, input)| Second {
                t: *t,
                change: *change,
                input: *input != 0,
            })
            .collect();
        let timeline = chain::timeline_hash(&seconds);
        if chain::to_hex(&timeline) != entry.timeline {
            return fail("timeline was changed after recording");
        }
        let link = chain::link(&previous, entry.start, entry.end, &video, &timeline);
        if chain::to_hex(&link) != entry.link {
            return fail("link hash does not match");
        }
        previous = link;
    }
    Ok(Verified { links, pruned })
}
