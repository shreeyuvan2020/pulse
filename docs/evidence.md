# Submitting hours and the `pulse.evidence.v1` document

This is what a program's review tool (such as Ari) reads. A program never sees anything a person did not submit, and every read is recorded where the person can see it.

## How hours get submitted

1. **Commit as you record.** The desktop client sends each 5-minute segment's hash-chain link to the server (`POST /api/chain`) shortly after the segment is recorded. The server stamps the time it arrived (`committed_at`). A link carries only hashes and times, no video.
2. **Choose what to submit.** `pulse submit <program> <project>` names the segments to submit. The server only accepts links it already holds for that person. Every other segment inside the same time range is listed as **omitted**, so leaving something out shows up as a gap instead of a quietly shorter timeline.
3. **Upload and verify.** Each video and each per-second timeline is uploaded and must hash to exactly what was committed in step 1. A video or timeline that was changed, or made up afterwards, is refused.
4. **Finalize.** The server checks that every segment arrived and freezes the totals.

A segment's hours can be submitted only once. Withdrawing a submission deletes its recordings and frees the hours.

## What the totals mean

- `recorded_seconds`: seconds of submitted recording.
- `active_seconds`: seconds where there was input **and** the screen changed within the last 120 seconds. A mouse jiggler moves input but not the screen, and a video playing on its own has no input, so neither counts. A second recorded twice (two computers) counts once. The rule is `active_seconds` in `crates/pulse-core/src/activity.rs`.
- `omitted_seconds`: time inside the range that was recorded but not submitted.
- `claimed_seconds`: what the person's profile says they worked on the project in that range, from the credits their computers reported. A big gap from `active_seconds` is worth a look.

These numbers describe what the recording shows. They do not say whether the time was *productive struggle*. That is for a reviewer to judge, with the evidence below.

## The document

`GET /api/programs/submissions/{id}/evidence` with `Authorization: Bearer <the program's key>`:

```json
{
  "format": "pulse.evidence.v1",
  "submission": { "id": 7, "program": "ari", "status": "submitted", "submitted_at": 1795140000,
                  "project": "github.com/rowan/cs2-web", "project_url": "https://github.com/rowan/cs2-web" },
  "maker": { "username": "rowan", "display_name": "Rowan", "github_login": "rowan-gh", "profile_url": null },
  "span": { "start": 1795130000, "end": 1795139000 },
  "totals": { "recorded_seconds": 8400, "active_seconds": 7900, "claimed_seconds": 8100, "omitted_seconds": 300 },
  "checks": {
    "video_hashes_verified": true, "timelines_verified": true, "every_segment_in_range_listed": true,
    "segments": 28, "omitted_segments": 1,
    "late_after_seconds": 3600, "segments_committed_late": 0, "max_commit_delay_seconds": 70
  },
  "runs": [ { "start": 1795130000, "end": 1795133000, "state": "active" } ],
  "slots_utc": { "2026-11-19": { "260": 300 } },
  "segments": [ { "link": "<64 hex>", "start": 1795130000, "end": 1795130300, "omitted": false,
                 "committed_at": 1795130340,
                 "video_path": "/api/programs/submissions/7/segments/<link>/video",
                 "timeline_path": "/api/programs/submissions/7/segments/<link>/timeline" } ],
  "credits": [ { "slot": 1795130100, "seconds": 300, "prompts": 4, "sources": ["claude-code"], "models": ["claude-opus-4-5"] } ],
  "pushes": [ { "repo": "rowan/cs2-web", "ref": "refs/heads/main", "head_sha": "<sha>", "commit_shas": ["<sha>"], "forced": false, "at": 1795130900 } ]
}
```

- **`runs`**: one strip for the whole submission, in time order. `active` and `idle` are recorded time; `omitted` was not submitted. Time with no run at all was not recorded. A review tool can draw this as the timeline and let a reviewer deflate ranges of it, in whole seconds.
- **`slots_utc`**: active seconds per UTC day and 5-minute slot (0-287), the same shape as the Hackatime heatmap Ari already renders.
- **`checks`**: every verification was enforced before the submission could be finalized; the document reports them so a reviewer does not have to redo them. `segments_committed_late` counts segments whose link reached the server more than an hour after they ended, which means they were not committed in real time. What to do about that is the program's call.
- **`credits`** and **`pushes`**: what the person's computers reported, and the pushes GitHub told the server about (never commit dates, which anyone can set). Compare them with the recording.
- **Videos** stream from `video_path` with HTTP range requests, so a player can seek. 1 frame per second, so a reviewer can watch an hour in a minute. `timeline_path` returns `{ "seconds": [[t, change, input], ...] }`: `change` is how much of the screen changed that second (0 to 1000) and `input` is whether there was keyboard or mouse input, never which keys.

## Other endpoints

- `GET /api/programs/submissions?since=<unix>`: submissions of this program that changed since then, including `withdrawn` ones (treat those as revoked), oldest first, 100 at a time.

## What the person sees

Every read of a submission's document, video or timeline is recorded with the program's name and shown to the person in `GET /api/submissions`. Repeated reads of the same thing within 10 minutes count as one.

## Configuration

`PULSE_STORAGE_DIR` is where recordings are kept (submissions are off without it). `PULSE_PROGRAM_KEYS=name=key,other=key` lists the programs, each with a random key of at least 32 characters. A key only sees the submissions made to its own program.
