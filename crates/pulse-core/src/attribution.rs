//! Turns agent events and input activity into credited seconds.
//!
//! A second is credited to a project when the person was present (some input
//! in the last `presence_seconds`) and engaged with that project's agent: from
//! their first prompt or session start, until `review_seconds` after the last
//! thing that happened in the session, and for the whole of every agent turn.
//! When several projects are engaged at once, the second goes to the one the
//! person touched most recently, so parallel agents never multiply hours.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::event::{Event, EventKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    /// How long one input keeps the person counted as present.
    pub presence_seconds: i64,
    /// How long after the last agent or human event the person is assumed to
    /// still be reading, reviewing or testing.
    pub review_seconds: i64,
    /// How long a single agent event counts as the agent working, when the
    /// turn's end is not known yet.
    pub agent_tail_seconds: i64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            presence_seconds: 120,
            review_seconds: 600,
            agent_tail_seconds: 60,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProjectTotals {
    pub credited_seconds: i64,
    pub prompts: u32,
    pub sources: BTreeSet<String>,
    pub models: BTreeSet<String>,
    /// Credited seconds per 5-minute slot, keyed by the slot's start time.
    pub slots: BTreeMap<i64, i64>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    pub projects: BTreeMap<String, ProjectTotals>,
    /// Seconds an agent worked while nobody was at the computer. Never credited.
    pub agent_only_seconds: i64,
}

impl Report {
    pub fn credited_seconds(&self) -> i64 {
        self.projects.values().map(|p| p.credited_seconds).sum()
    }
}

struct Windows {
    /// Half-open `[start, end)` intervals per project.
    engaged: BTreeMap<String, Vec<(i64, i64)>>,
    agent_running: Vec<(i64, i64)>,
    /// Every human event as `(time, project)`, sorted by time.
    human: Vec<(i64, String)>,
}

fn windows(events: &[Event], params: &Params) -> Windows {
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by_key(|event| event.t);

    let mut engaged: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    let mut agent_running = Vec::new();
    let mut human = Vec::new();
    // a session only counts once a person has acted in it
    let mut armed: HashSet<(&str, &str)> = HashSet::new();
    let mut open_turns: HashMap<(&str, &str), (i64, &str)> = HashMap::new();

    for event in sorted {
        let session = (event.source.as_str(), event.session.as_str());
        if event.kind.is_human() {
            armed.insert(session);
            human.push((event.t, event.project.clone()));
        }
        if armed.contains(&session) {
            engaged
                .entry(event.project.clone())
                .or_default()
                .push((event.t, event.t + params.review_seconds));
        }
        match event.kind {
            EventKind::PromptSubmitted => {
                open_turns
                    .entry(session)
                    .or_insert((event.t, event.project.as_str()));
            }
            EventKind::TurnEnded | EventKind::SessionEnded => {
                if let Some((start, project)) = open_turns.remove(&session) {
                    agent_running.push((start, event.t));
                    engaged
                        .entry(project.to_owned())
                        .or_default()
                        .push((start, event.t));
                }
            }
            EventKind::AgentActivity => {
                agent_running.push((event.t, event.t + params.agent_tail_seconds));
            }
            EventKind::SessionStarted | EventKind::ToolDecision => {}
        }
    }

    Windows {
        engaged,
        agent_running,
        human,
    }
}

fn coverage(intervals: impl IntoIterator<Item = (i64, i64)>, from: i64, to: i64) -> Vec<bool> {
    let len = (to - from).max(0) as usize;
    let mut diff = vec![0i32; len + 1];
    for (start, end) in intervals {
        let start = start.clamp(from, to);
        let end = end.clamp(from, to);
        if start < end {
            diff[(start - from) as usize] += 1;
            diff[(end - from) as usize] -= 1;
        }
    }
    let mut depth = 0;
    diff[..len]
        .iter()
        .map(|change| {
            depth += change;
            depth > 0
        })
        .collect()
}

/// Credits every second in `[from, to)`. `events` should include events from
/// before `from` (an hour is plenty) so windows that started earlier count.
/// `inputs` are times when the OS saw keyboard or mouse input.
pub fn attribute(events: &[Event], inputs: &[i64], from: i64, to: i64, params: &Params) -> Report {
    let windows = windows(events, params);

    // a human event is input too, so presence still works where the OS idle timer is unavailable
    let input_times = inputs
        .iter()
        .copied()
        .chain(windows.human.iter().map(|(t, _)| *t));
    let present = coverage(
        input_times.map(|t| (t, t + params.presence_seconds)),
        from,
        to,
    );
    let agent_running = coverage(windows.agent_running.iter().copied(), from, to);
    let engaged: Vec<(&str, Vec<bool>)> = windows
        .engaged
        .iter()
        .map(|(project, intervals)| {
            (
                project.as_str(),
                coverage(intervals.iter().copied(), from, to),
            )
        })
        .collect();

    let mut report = Report::default();
    for event in events.iter().filter(|e| e.t >= from && e.t < to) {
        let totals = report.projects.entry(event.project.clone()).or_default();
        if event.kind == EventKind::PromptSubmitted {
            totals.prompts += 1;
        }
        totals.sources.insert(event.source.clone());
        if let Some(model) = &event.model {
            totals.models.insert(model.clone());
        }
    }

    let mut last_human: HashMap<&str, i64> = HashMap::new();
    let mut next_human = 0;
    for offset in 0..present.len() {
        let second = from + offset as i64;
        while let Some((t, project)) = windows.human.get(next_human) {
            if *t > second {
                break;
            }
            last_human.insert(project.as_str(), *t);
            next_human += 1;
        }

        if present[offset] {
            let focused = engaged
                .iter()
                .filter(|(_, covered)| covered[offset])
                .map(|(project, _)| *project)
                .max_by_key(|project| (last_human.get(project).copied(), Reverse(*project)));
            if let Some(project) = focused {
                let totals = report.projects.entry(project.to_owned()).or_default();
                totals.credited_seconds += 1;
                *totals
                    .slots
                    .entry(second - second.rem_euclid(300))
                    .or_default() += 1;
            }
        } else if agent_running[offset] {
            report.agent_only_seconds += 1;
        }
    }
    report
}

/// Projects whose agent session is open at `now`. The recorder runs while
/// this is non-empty.
pub fn engaged_projects(events: &[Event], now: i64, params: &Params) -> BTreeSet<String> {
    windows(events, params)
        .engaged
        .into_iter()
        .filter(|(_, intervals)| {
            intervals
                .iter()
                .any(|(start, end)| *start <= now && now < *end)
        })
        .map(|(project, _)| project)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::PROTOCOL_VERSION;

    fn event(t: i64, project: &str, kind: EventKind) -> Event {
        Event {
            v: PROTOCOL_VERSION,
            t,
            source: "claude-code".into(),
            session: format!("session-{project}"),
            project: project.into(),
            kind,
            model: None,
        }
    }

    fn inputs(from: i64, to_inclusive: i64) -> Vec<i64> {
        (from..=to_inclusive).step_by(5).collect()
    }

    fn credited(report: &Report, project: &str) -> i64 {
        report
            .projects
            .get(project)
            .map_or(0, |p| p.credited_seconds)
    }

    #[test]
    fn flow_session_counts_until_review_window_closes() {
        let mut events = vec![event(0, "game", EventKind::PromptSubmitted)];
        events.extend(
            (60..1800)
                .step_by(60)
                .map(|t| event(t, "game", EventKind::AgentActivity)),
        );
        events.push(event(1800, "game", EventKind::TurnEnded));

        let report = attribute(&events, &inputs(0, 2395), 0, 4000, &Params::default());

        // engaged until 1800 + 600, present until 2395 + 120
        assert_eq!(credited(&report, "game"), 2400);
        assert_eq!(report.agent_only_seconds, 0);
        assert_eq!(report.projects["game"].prompts, 1);
    }

    #[test]
    fn agent_left_running_overnight_is_agent_time_not_credit() {
        let mut events = vec![event(0, "game", EventKind::PromptSubmitted)];
        events.extend(
            (60..28800)
                .step_by(60)
                .map(|t| event(t, "game", EventKind::AgentActivity)),
        );
        events.push(event(28800, "game", EventKind::TurnEnded));

        let report = attribute(&events, &[0], 0, 30000, &Params::default());

        assert_eq!(credited(&report, "game"), 120);
        assert_eq!(report.agent_only_seconds, 28800 - 120);
    }

    #[test]
    fn five_minutes_before_school_counts() {
        let events = vec![
            event(0, "game", EventKind::PromptSubmitted),
            event(30, "game", EventKind::AgentActivity),
            event(60, "game", EventKind::TurnEnded),
        ];

        let report = attribute(&events, &inputs(0, 300), 0, 3600, &Params::default());

        assert_eq!(credited(&report, "game"), 420);
    }

    #[test]
    fn overlapping_projects_go_to_most_recent_prompt_without_double_counting() {
        let events = vec![
            event(0, "a", EventKind::PromptSubmitted),
            event(300, "b", EventKind::PromptSubmitted),
            event(900, "a", EventKind::PromptSubmitted),
        ];

        let report = attribute(&events, &inputs(0, 2000), 0, 3000, &Params::default());

        // a: 0..300 and 900..1500, b: 300..900 even though a was still engaged until 600
        assert_eq!(credited(&report, "a"), 900);
        assert_eq!(credited(&report, "b"), 600);
        assert_eq!(report.credited_seconds(), 1500);
    }

    #[test]
    fn agent_activity_without_a_person_acting_is_never_credited() {
        let events: Vec<Event> = (0..600)
            .step_by(30)
            .map(|t| event(t, "bot", EventKind::AgentActivity))
            .collect();

        let report = attribute(&events, &inputs(0, 600), 0, 1200, &Params::default());

        assert_eq!(report.credited_seconds(), 0);
    }

    #[test]
    fn credit_is_split_into_five_minute_slots() {
        let events = vec![event(290, "game", EventKind::PromptSubmitted)];

        let report = attribute(&events, &[], 0, 1200, &Params::default());

        let slots: Vec<(i64, i64)> = report.projects["game"].slots.clone().into_iter().collect();
        assert_eq!(slots, vec![(0, 10), (300, 110)]);
    }

    #[test]
    fn prompts_before_the_report_window_still_decide_focus() {
        let events = vec![
            event(-100, "a", EventKind::PromptSubmitted),
            event(-50, "b", EventKind::PromptSubmitted),
        ];

        let report = attribute(&events, &inputs(-100, 100), 0, 200, &Params::default());

        assert_eq!(credited(&report, "a"), 0);
        assert_eq!(credited(&report, "b"), 200);
    }

    #[test]
    fn opening_an_agent_starts_a_session_for_the_review_window() {
        let events = vec![event(0, "game", EventKind::SessionStarted)];
        let params = Params::default();

        assert!(engaged_projects(&events, 599, &params).contains("game"));
        assert!(engaged_projects(&events, 600, &params).is_empty());
    }
}
