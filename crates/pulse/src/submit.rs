//! `pulse submit`: sends the recordings of a project to a program, so a
//! reviewer can see the work behind the hours. Nothing leaves the computer
//! until you confirm, and only the segments you submit are uploaded.
//!
//! A recording shows the whole screen, so a segment that also covers another
//! project shows whatever was open then. Those segments are counted and named
//! before you confirm, and `--skip` leaves any segment out. Leaving one out
//! is not hidden: the server lists it as omitted, with its time, in what the
//! program sees.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate};

use crate::now;
use crate::recorder::{Link, read_chain, timeline_file};
use crate::report::{format_duration, local_midnight};
use crate::store::Store;
use crate::sync::{self, Connection, call};

pub struct Options {
    pub program: String,
    pub project: String,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    /// Start times (unix seconds) of segments to leave out.
    pub skip: Vec<i64>,
    pub dry_run: bool,
    pub yes: bool,
}

pub struct Candidate<'a> {
    pub link: &'a Link,
    pub bytes: u64,
    /// Other projects that were open during the same segment.
    pub others: BTreeSet<&'a str>,
}

/// Why segments of the project are not offered.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Left {
    /// Video deleted to save space, so there is nothing to upload.
    pub deleted: usize,
    /// Already part of a submission.
    pub used: usize,
    pub skipped: usize,
}

/// The segments to submit: the project's, inside the date range, with a video
/// still on disk, not submitted before and not skipped.
pub fn select<'a>(
    links: &'a [Link],
    project: &str,
    range: (i64, i64),
    used: &BTreeSet<String>,
    skip: &[i64],
    video_size: &dyn Fn(&Link) -> Option<u64>,
) -> (Vec<Candidate<'a>>, Left) {
    let mut chosen = Vec::new();
    let mut left = Left::default();
    for link in links {
        if !link.projects.contains(project) || link.start < range.0 || link.end > range.1 {
            continue;
        }
        if used.contains(&link.link) {
            left.used += 1;
        } else if skip.contains(&link.start) {
            left.skipped += 1;
        } else if let Some(bytes) = video_size(link) {
            chosen.push(Candidate {
                link,
                bytes,
                others: link
                    .projects
                    .iter()
                    .map(String::as_str)
                    .filter(|other| *other != project)
                    .collect(),
            });
        } else {
            left.deleted += 1;
        }
    }
    (chosen, left)
}

fn local_time(unix: i64) -> String {
    DateTime::from_timestamp(unix, 0)
        .map(|time| {
            time.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

fn confirm(question: &str) -> Result<bool, String> {
    eprint!("{question} [y/N] ");
    io::stderr().flush().map_err(|error| error.to_string())?;
    let mut answer = String::new();
    io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

pub fn run(store: &Store, options: &Options) -> Result<(), String> {
    let connection =
        sync::load(store.root()).ok_or("not connected, run `pulse login <server>` first")?;
    let show = |error: sync::SyncError| error.to_string();

    // the server only accepts segments it already holds, so bring it up to date first
    let synced = sync::run_once(store.root(), &connection, now(), true).map_err(show)?;
    for reason in &synced.rejected {
        eprintln!("pulse: the server refused something while syncing: {reason}");
    }

    let programs = call(
        &connection,
        "GET",
        "/api/programs",
        "application/json",
        vec![],
        Duration::from_secs(15),
    )
    .map_err(show)?;
    let programs: serde_json::Value =
        serde_json::from_str(&programs).map_err(|_| "the server's reply made no sense")?;
    if programs["enabled"] != true {
        return Err("this server does not take submissions".into());
    }
    if !programs["programs"]
        .as_array()
        .is_some_and(|list| list.iter().any(|name| name == options.program.as_str()))
    {
        return Err(format!(
            "{} is not a program this server takes submissions for (it takes: {})",
            options.program,
            programs["programs"]
                .as_array()
                .map(|list| list
                    .iter()
                    .filter_map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ));
    }

    let used = call(
        &connection,
        "GET",
        "/api/chain/used",
        "application/json",
        vec![],
        Duration::from_secs(15),
    )
    .map_err(show)?;
    let used: BTreeSet<String> = serde_json::from_str::<serde_json::Value>(&used)
        .ok()
        .and_then(|json| serde_json::from_value(json["used"].clone()).ok())
        .ok_or("the server's reply made no sense")?;

    let recordings = store.root().join("recordings");
    let links = read_chain(&recordings);
    let range = (
        options.from.map_or(i64::MIN, local_midnight),
        options
            .to
            .and_then(|day| day.succ_opt())
            .map_or(i64::MAX, local_midnight),
    );
    let size = |link: &Link| {
        fs::metadata(recordings.join(&link.file))
            .ok()
            .map(|meta| meta.len())
    };
    let (chosen, left) = select(&links, &options.project, range, &used, &options.skip, &size);
    if chosen.is_empty() {
        return Err(format!(
            "nothing to submit for {}: no recordings of it are left to submit{}",
            options.project,
            if left.deleted + left.used + left.skipped > 0 {
                format!(
                    " ({} deleted to save space, {} already submitted, {} skipped)",
                    left.deleted, left.used, left.skipped
                )
            } else {
                " (record while a coding agent session is open on it first)".into()
            }
        ));
    }

    let seconds: i64 = chosen.iter().map(|c| c.link.end - c.link.start).sum();
    let total: u64 = chosen.iter().map(|c| c.bytes).sum();
    println!(
        "{} segments of {}: {} of recording, {}",
        chosen.len(),
        options.project,
        format_duration(seconds),
        megabytes(total)
    );
    println!(
        "  from {} to {}",
        local_time(chosen.first().expect("not empty").link.start),
        local_time(chosen.last().expect("not empty").link.end)
    );
    if left.deleted + left.used + left.skipped > 0 {
        println!(
            "  not included: {} deleted to save space, {} already submitted, {} skipped",
            left.deleted, left.used, left.skipped
        );
    }
    let mixed: Vec<&Candidate> = chosen.iter().filter(|c| !c.others.is_empty()).collect();
    if !mixed.is_empty() {
        println!();
        println!(
            "{} segments also covered other projects, so they show whatever else you had open:",
            mixed.len()
        );
        for candidate in mixed.iter().take(10) {
            println!(
                "  {}  skip with --skip {}  (also: {})",
                local_time(candidate.link.start),
                candidate.link.start,
                candidate
                    .others
                    .iter()
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if mixed.len() > 10 {
            println!("  ... and {} more", mixed.len() - 10);
        }
    }
    println!();
    println!(
        "The program's reviewers will be able to watch these recordings and see every time they do. You can withdraw the submission later, which deletes them from the server."
    );
    if options.dry_run {
        println!("(dry run: nothing was uploaded)");
        return Ok(());
    }
    if !options.yes
        && !confirm(&format!(
            "Upload {} to {}?",
            megabytes(total),
            options.program
        ))?
    {
        println!("Nothing uploaded.");
        return Ok(());
    }

    let body = serde_json::json!({
        "program": options.program,
        "project": options.project,
        "links": chosen.iter().map(|c| &c.link.link).collect::<Vec<_>>(),
    });
    let made = call(
        &connection,
        "POST",
        "/api/submissions",
        "application/json",
        body.to_string().into_bytes(),
        Duration::from_secs(60),
    )
    .map_err(show)?;
    let made: serde_json::Value =
        serde_json::from_str(&made).map_err(|_| "the server's reply made no sense")?;
    let id = made["id"]
        .as_i64()
        .ok_or("the server's reply made no sense")?;
    if let Some(omitted) = made["omitted"].as_i64().filter(|count| *count > 0) {
        println!("{omitted} segments inside this range are left out and will show as omitted.");
    }

    for (index, candidate) in chosen.iter().enumerate() {
        upload(&connection, id, candidate, &recordings).map_err(|error| {
            format!("{error}. Nothing is final yet: run the same command again to start over")
        })?;
        print!("\ruploaded {}/{}", index + 1, chosen.len());
        let _ = io::stdout().flush();
    }
    println!();
    let done = call(
        &connection,
        "POST",
        &format!("/api/submissions/{id}/finalize"),
        "application/json",
        vec![],
        Duration::from_secs(60),
    )
    .map_err(show)?;
    let done: serde_json::Value =
        serde_json::from_str(&done).map_err(|_| "the server's reply made no sense")?;
    let minutes = |key: &str| format_duration(done[key].as_i64().unwrap_or(0));
    println!(
        "Submitted to {}: {} recorded, {} of it active ({} claimed on your profile).",
        options.program,
        minutes("recorded_seconds"),
        minutes("active_seconds"),
        minutes("claimed_seconds")
    );
    println!(
        "You can see what the program looks at on your profile page, and withdraw the submission there."
    );
    Ok(())
}

fn upload(
    connection: &Connection,
    id: i64,
    candidate: &Candidate,
    dir: &Path,
) -> Result<(), String> {
    let link = &candidate.link.link;
    let video = fs::read(dir.join(&candidate.link.file))
        .map_err(|error| format!("cannot read {}: {error}", candidate.link.file))?;
    let timeline = fs::read(dir.join(timeline_file(&candidate.link.file))).map_err(|error| {
        format!(
            "cannot read the timeline of {}: {error}",
            candidate.link.file
        )
    })?;
    let base = format!("/api/submissions/{id}/segments/{link}");
    // a 5 minute segment is a few MB, which even a slow connection sends in under 5 minutes: 300 s
    call(
        connection,
        "PUT",
        &format!("{base}/video"),
        "video/mp4",
        video,
        Duration::from_secs(300),
    )
    .map_err(|error| error.to_string())?;
    call(
        connection,
        "PUT",
        &format!("{base}/timeline"),
        "application/json",
        timeline,
        Duration::from_secs(60),
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(start: i64, hash: char, projects: &[&str]) -> Link {
        Link {
            start,
            end: start + 300,
            file: format!("{start}.mp4"),
            video: hash.to_string().repeat(64),
            timeline: hash.to_string().repeat(64),
            previous: "0".repeat(64),
            link: hash.to_string().repeat(64),
            projects: projects.iter().map(|name| name.to_string()).collect(),
        }
    }

    #[test]
    fn picks_this_projects_segments_and_says_why_others_were_left_out() {
        let links = vec![
            link(1000, 'a', &["game"]),
            link(1300, 'b', &["game", "homework"]),
            link(1600, 'c', &["homework"]),
            link(1900, 'd', &["game"]),
            link(2200, 'e', &["game"]),
            link(2500, 'f', &["game"]),
            link(9000, 'g', &["game"]),
        ];
        let used: BTreeSet<String> = ["d".repeat(64)].into();
        // the video of 'e' was deleted to save space
        let size = |link: &Link| (link.start != 2200).then_some(100);

        let (chosen, left) = select(&links, "game", (0, 5000), &used, &[2500], &size);

        let starts: Vec<i64> = chosen.iter().map(|c| c.link.start).collect();
        assert_eq!(starts, vec![1000, 1300]);
        assert_eq!(
            left,
            Left {
                deleted: 1,
                used: 1,
                skipped: 1
            }
        );
        // the segment that also covered homework is flagged so its screen is not a surprise
        assert!(chosen[0].others.is_empty());
        assert_eq!(
            chosen[1].others.iter().copied().collect::<Vec<_>>(),
            ["homework"]
        );
    }

    #[test]
    fn a_segment_must_lie_wholly_inside_the_date_range() {
        let links = vec![link(1000, 'a', &["p"]), link(1100, 'b', &["p"])];
        let size = |_: &Link| Some(1);
        // 'b' ends at 1400, one second past the range end
        let (chosen, _) = select(&links, "p", (1000, 1399), &BTreeSet::new(), &[], &size);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].link.start, 1000);
        // 'a' starts one second before the range start
        let (chosen, _) = select(&links, "p", (1001, 9999), &BTreeSet::new(), &[], &size);
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].link.start, 1100);
    }
}
