//! Turns the segments of a submission into the numbers and the timeline a
//! reviewer sees. It decides nothing about whether the hours are deserved: it
//! reports what the recording shows, second by second.

use std::collections::BTreeMap;

use pulse_core::Params;
use pulse_core::activity::active_seconds;
use pulse_core::chain::Second;
use serde::Serialize;

use crate::stats::{DAY, date_string};

pub struct Segment {
    pub start: i64,
    pub end: i64,
    pub omitted: bool,
    /// Empty for omitted segments, whose recording was not submitted.
    pub seconds: Vec<Second>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Run {
    pub start: i64,
    /// Exclusive.
    pub end: i64,
    /// `active`, `idle` (recorded, but nothing was happening) or `omitted`
    /// (not submitted). Time with no run at all was not recorded.
    pub state: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Analysis {
    pub recorded_seconds: i64,
    pub active_seconds: i64,
    pub omitted_seconds: i64,
    pub runs: Vec<Run>,
    /// Active seconds per UTC day and 5-minute slot, in the shape
    /// `{ "YYYY-MM-DD": { "<slot 0-287>": seconds } }`.
    pub slots_utc: BTreeMap<String, BTreeMap<String, i64>>,
}

pub fn analyze(segments: &[Segment]) -> Analysis {
    let submitted: Vec<Second> = segments
        .iter()
        .filter(|segment| !segment.omitted)
        .flat_map(|segment| segment.seconds.iter().copied())
        .collect();
    let seconds = active_seconds(&submitted, Params::default().presence_seconds);

    let mut runs: Vec<Run> = Vec::new();
    let mut slots_utc: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    let mut active_total = 0;
    for (t, active) in &seconds {
        let state = if *active { "active" } else { "idle" };
        match runs.last_mut() {
            Some(run) if run.state == state && run.end == *t => run.end = t + 1,
            _ => runs.push(Run {
                start: *t,
                end: t + 1,
                state,
            }),
        }
        if *active {
            active_total += 1;
            let slot = (t.rem_euclid(DAY) / 300).to_string(); // 300 seconds per slot
            *slots_utc
                .entry(date_string(t.div_euclid(DAY)))
                .or_default()
                .entry(slot)
                .or_default() += 1;
        }
    }
    let mut omitted_seconds = 0;
    for segment in segments.iter().filter(|segment| segment.omitted) {
        omitted_seconds += segment.end - segment.start;
        runs.push(Run {
            start: segment.start,
            end: segment.end,
            state: "omitted",
        });
    }
    runs.sort_by_key(|run| (run.start, run.end));
    Analysis {
        recorded_seconds: seconds.len() as i64,
        active_seconds: active_total,
        omitted_seconds,
        runs,
        slots_utc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn working(from: i64, to: i64) -> Vec<Second> {
        (from..to)
            .map(|t| Second {
                t,
                change: 30,
                input: true,
            })
            .collect()
    }

    #[test]
    fn counts_active_time_merges_runs_and_buckets_it_by_utc_slot() {
        // 2025-11-19T00:04:00Z is 20411 * 86400 + 240; the work crosses a slot boundary at +300
        let day = 20_411 * DAY;
        let start = day + 240;
        let mut idle = working(start + 100, start + 105);
        for second in &mut idle {
            second.change = 0;
            second.input = false;
        }
        let segments = vec![
            Segment {
                start,
                end: start + 100,
                omitted: false,
                seconds: working(start, start + 100),
            },
            Segment {
                start: start + 100,
                end: start + 105,
                omitted: false,
                seconds: idle.clone(),
            },
            Segment {
                start: start + 500,
                end: start + 800,
                omitted: true,
                seconds: vec![],
            },
        ];
        let analysis = analyze(&segments);

        // the 5 idle seconds right after work are still inside the 120 second window, so active
        assert_eq!(
            (
                analysis.recorded_seconds,
                analysis.active_seconds,
                analysis.omitted_seconds
            ),
            (105, 105, 300)
        );
        assert_eq!(
            analysis.runs,
            vec![
                Run {
                    start,
                    end: start + 105,
                    state: "active"
                },
                Run {
                    start: start + 500,
                    end: start + 800,
                    state: "omitted"
                },
            ]
        );
        // 00:04:00..00:04:59 is slot 0 (60 s), 00:05:00.. is slot 1 (45 s)
        let slots = &analysis.slots_utc["2025-11-19"];
        assert_eq!((slots["0"], slots["1"]), (60, 45));
        let sum: i64 = analysis
            .slots_utc
            .values()
            .flat_map(|day| day.values())
            .sum();
        assert_eq!(sum, analysis.active_seconds);
    }

    #[test]
    fn idle_stretches_are_visible_and_never_counted() {
        let mut quiet = working(1000, 1400);
        for second in &mut quiet {
            second.change = 0;
            second.input = false;
        }
        let mut seconds = working(0, 10);
        seconds.extend(quiet);
        let analysis = analyze(&[Segment {
            start: 0,
            end: 1400,
            omitted: false,
            seconds,
        }]);
        // active until the window after the last input/change at t=9 closes: t < 129
        assert_eq!(analysis.active_seconds, 10);
        assert_eq!(
            analysis.runs.first(),
            Some(&Run {
                start: 0,
                end: 10,
                state: "active"
            })
        );
        assert_eq!(
            analysis.runs.last(),
            Some(&Run {
                start: 1000,
                end: 1400,
                state: "idle"
            })
        );
    }

    #[test]
    fn omitted_segments_contribute_no_seconds_even_if_they_carry_some() {
        let analysis = analyze(&[Segment {
            start: 0,
            end: 10,
            omitted: true,
            seconds: working(0, 10),
        }]);
        assert_eq!(
            (
                analysis.recorded_seconds,
                analysis.active_seconds,
                analysis.omitted_seconds
            ),
            (0, 0, 10)
        );
    }
}
