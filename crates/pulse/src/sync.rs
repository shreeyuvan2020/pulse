//! Sends credited time to the Pulse server. Only per-slot totals leave the
//! computer: a project id, a 5-minute slot, how many seconds of it were
//! credited, a prompt count and the tool and model names. Recordings,
//! transcripts and file paths never go through here.
//!
//! It also commits each recorded segment's hash-chain link (hashes and times
//! only, no video) so the server knows when a segment was recorded. That is
//! what makes a recording submitted later checkable.
//!
//! The same slot is sent again while it is still changing and the server
//! replaces its old numbers, so sending is safe to repeat and a computer that
//! was offline catches up by itself.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pulse_core::{Event, EventKind, Params, attribute};
use serde::{Deserialize, Serialize};

use crate::now;
use crate::recorder::{self, Link};
use crate::store::Store;

const SLOT: i64 = 300;
/// How far back to send on the first sync and after a long break. The server
/// refuses slots older than 7 days; 6 leaves room for a slow start.
const MAX_LOOKBACK: i64 = 518_400; // 6 * 24 * 60 * 60
/// Time is sent again for this long after it happens, because events can reach
/// the spool late and a long agent turn can add credit to earlier seconds.
const RESEND_WINDOW: i64 = 3600;
/// The server's limits per request.
const CREDITS_PER_REQUEST: usize = 300;
const LINKS_PER_REQUEST: usize = 200;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Connection {
    pub server: String,
    pub token: String,
}

fn config_path(root: &Path) -> PathBuf {
    root.join("config.json")
}

fn state_path(root: &Path) -> PathBuf {
    root.join("state").join("sync.json")
}

pub fn load(root: &Path) -> Option<Connection> {
    serde_json::from_slice(&fs::read(config_path(root)).ok()?).ok()
}

/// Writes the token where only this person's account can read it.
pub fn save(root: &Path, connection: &Connection) -> io::Result<()> {
    fs::create_dir_all(root)?;
    let path = config_path(root);
    let temporary = path.with_extension("json.tmp");
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    io::Write::write_all(
        &mut options.open(&temporary)?,
        &serde_json::to_vec_pretty(connection)?,
    )?;
    fs::rename(temporary, path)
}

/// Forgets the server. What was sent before is not sent again to a different
/// account, which starts from scratch.
pub fn remove(root: &Path) -> io::Result<bool> {
    let _ = fs::remove_file(state_path(root));
    match fs::remove_file(config_path(root)) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// `https://pulse.example.org`, or `http://localhost:8080` for development.
/// The token is a password, so plain `http` is only accepted for this
/// computer.
pub fn normalize_server(address: &str) -> Result<String, String> {
    let address = address.trim().trim_end_matches('/');
    let (scheme, rest) = address
        .split_once("://")
        .ok_or("the server address must start with https://")?;
    if rest.is_empty() || rest.contains(['/', '@', '?', '#', ' ']) {
        return Err("give the server address alone, like https://pulse.example.org".into());
    }
    let host = match rest.strip_prefix('[') {
        Some(ipv6) => ipv6.split_once(']').map_or("", |(host, _)| host),
        None => rest.split(':').next().unwrap_or(""),
    };
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1");
    match scheme {
        "https" => {}
        "http" if local => {}
        "http" => return Err("use https:// so your token cannot be read on the way".into()),
        _ => return Err("the server address must start with https://".into()),
    }
    Ok(format!("{scheme}://{rest}"))
}

#[derive(Serialize, Deserialize, Default)]
struct State {
    /// Everything before this slot is final and need not be sent again.
    synced_until: Option<i64>,
    last_upload: Option<i64>,
    /// How many lines of `chain.jsonl` the server has been sent.
    #[serde(default)]
    chain_sent: usize,
}

fn read_state(root: &Path) -> State {
    fs::read(state_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn write_state(root: &Path, state: &State) -> io::Result<()> {
    let path = state_path(root);
    fs::create_dir_all(path.parent().expect("state file has a folder"))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(state)?)?;
    fs::rename(temporary, path)
}

/// When time was last sent, as unix seconds.
pub fn last_upload(root: &Path) -> Option<i64> {
    read_state(root).last_upload
}

fn slot_start(t: i64) -> i64 {
    t - t.rem_euclid(SLOT)
}

/// The slots to send: from the first one that may still change up to a few
/// seconds ago. The end is kept behind the clock so a computer whose clock
/// runs a little fast is not refused for sending the future.
pub fn plan(synced_until: Option<i64>, now: i64) -> (i64, i64) {
    let to = now - 10;
    let oldest = slot_start(to - MAX_LOOKBACK);
    (synced_until.map_or(oldest, |slot| slot.max(oldest)), to)
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct Credit {
    project: String,
    slot: i64,
    seconds: i64,
    prompts: u32,
    sources: Vec<String>,
    models: Vec<String>,
}

/// A name the server accepts: no control characters, a limited length.
fn clean(name: &str, longest: usize) -> Option<String> {
    let name: String = name.trim().chars().take(longest).collect();
    (!name.is_empty() && !name.chars().any(char::is_control)).then_some(name)
}

fn labels(names: &BTreeSet<String>) -> Vec<String> {
    names
        .iter()
        .filter_map(|name| clean(name, 64))
        .take(8)
        .collect()
}

/// Turns the spool into what the server stores: credited seconds per project
/// and 5-minute slot. `from` must be the start of a slot, because a slot cut
/// short at its start would replace the server's good number with a smaller
/// one. `events` and `inputs` should reach back far enough for windows that
/// began earlier to count (the attribution rules say how far).
pub fn build_credits(events: &[Event], inputs: &[i64], from: i64, to: i64) -> Vec<Credit> {
    let report = attribute(events, inputs, from, to, &Params::default());
    let mut prompts: BTreeMap<(&str, i64), u32> = BTreeMap::new();
    for event in events {
        if event.kind == EventKind::PromptSubmitted && (from..to).contains(&event.t) {
            *prompts
                .entry((event.project.as_str(), slot_start(event.t)))
                .or_default() += 1;
        }
    }
    let mut credits = Vec::new();
    for (project, totals) in &report.projects {
        let Some(name) = clean(project, 200) else {
            continue;
        };
        let (sources, models) = (labels(&totals.sources), labels(&totals.models));
        for (slot, seconds) in &totals.slots {
            credits.push(Credit {
                project: name.clone(),
                slot: *slot,
                seconds: *seconds,
                prompts: prompts
                    .get(&(project.as_str(), *slot))
                    .copied()
                    .unwrap_or(0),
                sources: sources.clone(),
                models: models.clone(),
            });
        }
    }
    credits.sort_by(|a, b| (a.slot, &a.project).cmp(&(b.slot, &b.project)));
    credits
}

#[derive(Debug)]
pub enum SyncError {
    /// The server does not know this token: it was revoked or mistyped.
    Unauthorized,
    Failed(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Unauthorized => formatter.write_str("the server does not accept this token"),
            SyncError::Failed(message) => formatter.write_str(message),
        }
    }
}

#[derive(Deserialize)]
struct Reply {
    #[serde(default)]
    rejected: Vec<Rejection>,
}

#[derive(Deserialize)]
struct Rejection {
    reason: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub sent: usize,
    /// Recorded segments committed to the server.
    pub links: usize,
    /// The server's reasons for entries it refused. Those are not retried.
    pub rejected: Vec<String>,
}

/// One request to the server. `body` is sent as is. Failures carry the
/// server's own explanation when it gave one.
pub fn call(
    connection: &Connection,
    method: &str,
    path: &str,
    content_type: &str,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<String, SyncError> {
    let failed = |error: &dyn std::fmt::Display| SyncError::Failed(error.to_string());
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();
    let request = ureq::http::Request::builder()
        .method(method)
        .uri(format!("{}{path}", connection.server))
        .header("authorization", format!("Bearer {}", connection.token))
        .header("content-type", content_type)
        .body(body)
        .map_err(|error| failed(&error))?;
    let mut response = agent.run(request).map_err(|error| failed(&error))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|error| failed(&error))?;
    match status {
        200..=299 => Ok(text),
        401 => Err(SyncError::Unauthorized),
        _ => Err(SyncError::Failed(
            serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|json| json["error"].as_str().map(str::to_owned))
                .map_or_else(
                    || format!("the server answered {status}"),
                    |message| format!("the server said: {message}"),
                ),
        )),
    }
}

fn post_json(
    connection: &Connection,
    path: &str,
    body: &serde_json::Value,
) -> Result<Reply, SyncError> {
    let text = call(
        connection,
        "POST",
        path,
        "application/json",
        body.to_string().into_bytes(),
        Duration::from_secs(15),
    )?;
    serde_json::from_str(&text)
        .map_err(|_| SyncError::Failed("the server's reply made no sense".into()))
}

fn post(connection: &Connection, credits: &[Credit]) -> Result<Reply, SyncError> {
    post_json(
        connection,
        "/api/credits",
        &serde_json::json!({ "credits": credits }),
    )
}

fn post_links(connection: &Connection, links: &[Link]) -> Result<Reply, SyncError> {
    let links: Vec<_> = links
        .iter()
        .map(|link| {
            serde_json::json!({
                "start": link.start, "end": link.end, "previous": link.previous,
                "video": link.video, "timeline": link.timeline, "link": link.link,
            })
        })
        .collect();
    post_json(
        connection,
        "/api/chain",
        &serde_json::json!({ "links": links }),
    )
}

/// Checks that the server accepts the token, without sending any time.
pub fn verify(connection: &Connection) -> Result<(), SyncError> {
    post(connection, &[]).map(|_| ())
}

/// Sends whatever is new. With nothing to send it does not touch the network
/// unless `force` is set, which `pulse sync` uses to check the token.
pub fn run_once(
    root: &Path,
    connection: &Connection,
    now: i64,
    force: bool,
) -> Result<Outcome, SyncError> {
    let store = Store::new(root.to_path_buf());
    let state = read_state(root);
    let (from, to) = plan(state.synced_until, now);
    let mut outcome = Outcome::default();
    let mut last_upload = state.last_upload;
    if from < to {
        let credits = build_credits(
            &store.events(from - RESEND_WINDOW, to),
            &store.inputs(from - RESEND_WINDOW, to),
            from,
            to,
        );
        for chunk in credits.chunks(CREDITS_PER_REQUEST) {
            let reply = post(connection, chunk)?;
            outcome.sent += chunk.len();
            outcome
                .rejected
                .extend(reply.rejected.into_iter().map(|entry| entry.reason));
            last_upload = Some(now);
        }
        if credits.is_empty() && force {
            verify(connection)?;
        }
    } else if force {
        verify(connection)?;
    }
    let links = recorder::read_chain(&root.join("recordings"));
    let mut chain_sent = state.chain_sent.min(links.len());
    while chain_sent < links.len() {
        let chunk = &links[chain_sent..(chain_sent + LINKS_PER_REQUEST).min(links.len())];
        let reply = post_links(connection, chunk)?;
        outcome.links += chunk.len();
        outcome
            .rejected
            .extend(reply.rejected.into_iter().map(|entry| entry.reason));
        chain_sent += chunk.len();
        last_upload = Some(now);
    }
    let synced_until = Some(slot_start(to - RESEND_WINDOW).max(from));
    if synced_until != state.synced_until
        || last_upload != state.last_upload
        || chain_sent != state.chain_sent
    {
        let _ = write_state(
            root,
            &State {
                synced_until,
                last_upload,
                chain_sent,
            },
        );
    }
    Ok(outcome)
}

/// Uploads in the background, once a minute, for as long as the daemon runs.
/// It runs on its own thread so a slow network never delays the screen
/// recorder, and it reads the connection again each time, so `pulse login`
/// takes effect without restarting the daemon.
pub fn spawn(root: PathBuf, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut next = Instant::now() + Duration::from_secs(15);
        let mut last_message: Option<String> = None;
        let mut refused_token: Option<String> = None;
        while !stop.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_secs(1));
            if Instant::now() < next {
                continue;
            }
            next = Instant::now() + Duration::from_secs(60);
            let Some(connection) = load(&root) else {
                continue;
            };
            if refused_token.as_ref() == Some(&connection.token) {
                continue;
            }
            // each problem is said once, not every minute
            let message = match run_once(&root, &connection, now(), false) {
                Ok(outcome) if outcome.rejected.is_empty() => None,
                Ok(outcome) => Some(format!(
                    "the server refused {} entries: {}",
                    outcome.rejected.len(),
                    outcome.rejected[0]
                )),
                Err(SyncError::Unauthorized) => {
                    refused_token = Some(connection.token.clone());
                    Some("the server does not accept this computer's token any more. Run `pulse login` again".into())
                }
                Err(SyncError::Failed(reason)) => Some(format!(
                    "cannot upload time right now ({reason}); will try again"
                )),
            };
            match (&message, &last_message) {
                (Some(text), previous) if previous.as_ref() != Some(text) => {
                    eprintln!("pulse: {text}");
                }
                (None, Some(_)) => eprintln!("pulse: uploading time works again"),
                _ => {}
            }
            last_message = message;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulse_core::PROTOCOL_VERSION;
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    const START: i64 = 1_794_999_900; // a multiple of 300

    fn event(t: i64, project: &str, source: &str) -> Event {
        Event {
            v: PROTOCOL_VERSION,
            t,
            source: source.into(),
            session: format!("s-{project}"),
            project: project.into(),
            kind: EventKind::PromptSubmitted,
            model: (source == "codex").then(|| "gpt-5-codex".into()),
        }
    }

    /// game prompts at +200 and +320, a notes prompt at +330. Each prompt makes
    /// its person present for 120 s, so presence runs +200..+450. The most
    /// recently touched project gets the second: game until +330, notes after.
    fn scenario() -> Vec<Event> {
        vec![
            event(START + 200, "game", "claude-code"),
            event(START + 320, "game", "codex"),
            event(START + 330, "notes", "claude-code"),
        ]
    }

    #[test]
    fn credits_are_split_by_slot_with_prompts_counted_where_they_happened() {
        let credits = build_credits(&scenario(), &[], START, START + 600);

        let rows: Vec<(&str, i64, i64, u32)> = credits
            .iter()
            .map(|credit| {
                (
                    credit.project.as_str(),
                    credit.slot,
                    credit.seconds,
                    credit.prompts,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("game", START, 100, 1),
                ("game", START + 300, 30, 1),
                ("notes", START + 300, 120, 1),
            ]
        );
        assert_eq!(credits[0].sources, ["claude-code", "codex"]);
        assert_eq!(credits[0].models, ["gpt-5-codex"]);
        assert_eq!(credits[2].sources, ["claude-code"]);
        assert!(credits[2].models.is_empty());
    }

    #[test]
    fn names_the_server_would_refuse_are_cleaned_before_sending() {
        assert_eq!(
            clean("  github.com/o/r ", 200).as_deref(),
            Some("github.com/o/r")
        );
        assert_eq!(
            clean(&"x".repeat(300), 200).map(|name| name.len()),
            Some(200)
        );
        assert_eq!(clean("two\nlines", 200), None);
        assert_eq!(clean("   ", 200), None);
    }

    #[test]
    fn plan_never_reaches_back_further_than_the_server_accepts() {
        let now = 1_795_000_510;
        // to = now - 10; the oldest slot is 6 days (518400 s) before it
        assert_eq!(plan(None, now), (1_794_482_100, 1_795_000_500));
        assert_eq!(
            plan(Some(1_794_999_900), now),
            (1_794_999_900, 1_795_000_500)
        );
        assert_eq!(
            plan(Some(1_000_000_200), now),
            (1_794_482_100, 1_795_000_500)
        );
    }

    #[test]
    fn plain_http_is_only_for_this_computer() {
        for (given, expected) in [
            ("https://pulse.example.org/", "https://pulse.example.org"),
            ("http://localhost:8080", "http://localhost:8080"),
            ("http://127.0.0.1", "http://127.0.0.1"),
            ("http://[::1]:3000/", "http://[::1]:3000"),
        ] {
            assert_eq!(normalize_server(given).as_deref(), Ok(expected), "{given}");
        }
        for bad in [
            "http://pulse.example.org",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com:80",
            "https://user:secret@pulse.example.org",
            "https://pulse.example.org/some/path",
            "ftp://pulse.example.org",
            "pulse.example.org",
        ] {
            assert!(normalize_server(bad).is_err(), "{bad}");
        }
    }

    /// A one-request-per-connection server that answers with the given
    /// statuses and bodies, and reports what it was sent.
    fn stub(
        answers: Vec<(u16, &'static str)>,
    ) -> (String, mpsc::Receiver<(String, Option<String>, Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let (mut length, mut authorization) = (0, None);
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let line = line.trim_end().to_owned();
                    if line.is_empty() {
                        break;
                    }
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                    if lower.starts_with("authorization:") {
                        authorization = Some(line["authorization:".len()..].trim().to_owned());
                    }
                }
                let mut payload = vec![0; length];
                std::io::Read::read_exact(&mut reader, &mut payload).unwrap();
                let _ = sender.send((
                    request_line.trim_end().to_owned(),
                    authorization,
                    serde_json::from_slice(&payload).unwrap(),
                ));
                write!(
                    stream,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (address, receiver)
    }

    fn spool(root: &Path) {
        let store = Store::new(root.to_path_buf());
        for event in scenario() {
            store.append_event(&event).unwrap();
        }
    }

    fn connection(server: String) -> Connection {
        Connection {
            server,
            token: "pulse_abc".into(),
        }
    }

    #[test]
    fn sends_exactly_what_the_server_expects_and_remembers_it_only_when_it_worked() {
        let root = tempfile::tempdir().unwrap();
        spool(root.path());
        let now = START + 710; // to = START + 700, after both slots
        let (server, received) = stub(vec![
            (500, "{}"),
            (401, "{}"),
            (
                200,
                r#"{"accepted":2,"clamped":0,"rejected":[{"index":1,"reason":"slot is in the future"}]}"#,
            ),
        ]);
        let connection = connection(server);

        assert!(matches!(
            run_once(root.path(), &connection, now, false),
            Err(SyncError::Failed(_))
        ));
        assert!(matches!(
            run_once(root.path(), &connection, now, false),
            Err(SyncError::Unauthorized)
        ));
        assert_eq!(
            last_upload(root.path()),
            None,
            "a failed upload must not count as sent"
        );

        let outcome = run_once(root.path(), &connection, now, false).unwrap();
        assert_eq!(
            outcome,
            Outcome {
                sent: 3,
                rejected: vec!["slot is in the future".into()],
                ..Outcome::default()
            }
        );
        assert_eq!(last_upload(root.path()), Some(now));

        let (_, _, _) = received.recv().unwrap();
        let (_, _, _) = received.recv().unwrap();
        let (request, authorization, body) = received.recv().unwrap();
        assert_eq!(request, "POST /api/credits HTTP/1.1");
        assert_eq!(authorization.as_deref(), Some("Bearer pulse_abc"));
        assert_eq!(
            body,
            json!({ "credits": [
                { "project": "game", "slot": START, "seconds": 100, "prompts": 1,
                  "sources": ["claude-code", "codex"], "models": ["gpt-5-codex"] },
                { "project": "game", "slot": START + 300, "seconds": 30, "prompts": 1,
                  "sources": ["claude-code", "codex"], "models": ["gpt-5-codex"] },
                { "project": "notes", "slot": START + 300, "seconds": 120, "prompts": 1,
                  "sources": ["claude-code"], "models": [] },
            ] })
        );
    }

    #[test]
    fn with_nothing_to_send_the_network_is_left_alone_unless_asked() {
        let root = tempfile::tempdir().unwrap();
        let unreachable = connection("http://127.0.0.1:1".into());

        // nothing was recorded, so there is nothing to send and nobody to ask
        assert_eq!(
            run_once(root.path(), &unreachable, START + 710, false).unwrap(),
            Outcome::default()
        );
        assert!(matches!(
            run_once(root.path(), &unreachable, START + 710, true),
            Err(SyncError::Failed(_))
        ));
    }

    #[test]
    fn the_connection_file_round_trips_and_forgetting_it_resets_what_was_sent() {
        let root = tempfile::tempdir().unwrap();
        let saved = connection("https://pulse.example.org".into());
        save(root.path(), &saved).unwrap();
        assert_eq!(load(root.path()), Some(saved));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(config_path(root.path()))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o077,
                0,
                "the token must not be readable by other users"
            );
        }

        write_state(
            root.path(),
            &State {
                synced_until: Some(5),
                last_upload: Some(6),
                chain_sent: 7,
            },
        )
        .unwrap();
        assert!(remove(root.path()).unwrap());
        assert_eq!((load(root.path()), last_upload(root.path())), (None, None));
        assert!(!remove(root.path()).unwrap());
    }

    fn chain_line(start: i64, tag: char) -> String {
        let hash = |c: char| c.to_string().repeat(64);
        format!(
            r#"{{"start":{start},"end":{},"file":"{start}.mp4","video":"{}","timeline":"{}","previous":"{}","link":"{}","projects":["game"]}}"#,
            start + 300,
            hash(tag),
            hash('b'),
            hash('0'),
            hash('c'),
        )
    }

    #[test]
    fn recorded_segments_are_committed_once_each_with_only_hashes_and_times() {
        let root = tempfile::tempdir().unwrap();
        let recordings = root.path().join("recordings");
        fs::create_dir_all(&recordings).unwrap();
        let line = |start, tag| chain_line(start, tag);
        fs::write(
            recordings.join("chain.jsonl"),
            format!("{}\n{}\n", line(START, 'a'), line(START + 300, 'd')),
        )
        .unwrap();
        let (server, received) = stub(vec![(
            200,
            r#"{"accepted":1,"rejected":[{"index":1,"reason":"segment ends in the future"}]}"#,
        )]);
        let connection = connection(server);
        let now = START + 10_000;

        let outcome = run_once(root.path(), &connection, now, false).unwrap();
        assert_eq!(outcome.links, 2);
        assert_eq!(outcome.rejected, vec!["segment ends in the future"]);
        let (request, _, body) = received.recv().unwrap();
        assert_eq!(request, "POST /api/chain HTTP/1.1");
        let sent = body["links"].as_array().unwrap();
        assert_eq!(sent.len(), 2);
        let mut keys: Vec<&String> = sent[0].as_object().unwrap().keys().collect();
        keys.sort();
        // no file names and no project names: they are not the server's business
        assert_eq!(
            keys,
            ["end", "link", "previous", "start", "timeline", "video"]
        );
        assert_eq!(sent[1]["video"], "d".repeat(64));

        // nothing new, so nothing is sent (the stub has no second answer to give)
        assert_eq!(
            run_once(root.path(), &connection, now, false).unwrap(),
            Outcome::default()
        );
        // a segment recorded later is the only one sent next
        let mut chain = fs::read_to_string(recordings.join("chain.jsonl")).unwrap();
        chain.push_str(&format!("{}\n", line(START + 600, 'e')));
        fs::write(recordings.join("chain.jsonl"), chain).unwrap();
        let (server, received) = stub(vec![(200, r#"{"accepted":1,"rejected":[]}"#)]);
        let outcome = run_once(root.path(), &self::connection(server), now, false).unwrap();
        assert_eq!(outcome.links, 1);
        assert_eq!(
            received.recv().unwrap().2["links"][0]["video"],
            "e".repeat(64)
        );
    }

    #[test]
    fn the_servers_own_explanation_reaches_the_person() {
        let (server, _) = stub(vec![(
            400,
            r#"{"error":"that program does not take submissions"}"#,
        )]);
        let error = call(
            &connection(server),
            "POST",
            "/x",
            "application/json",
            b"{}".to_vec(),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "the server said: that program does not take submissions"
        );
    }
}
