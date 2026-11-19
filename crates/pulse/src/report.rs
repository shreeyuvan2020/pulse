use std::collections::BTreeMap;

use chrono::{DateTime, Local, NaiveDate, NaiveTime};
use pulse_core::{Params, Report, attribute};
use serde_json::{Value, json};

use crate::store::Store;

/// Start of a local calendar day as a unix timestamp.
pub fn local_midnight(date: NaiveDate) -> i64 {
    date.and_time(NaiveTime::MIN)
        .and_local_timezone(Local)
        .earliest()
        .expect("every day has a first moment")
        .timestamp()
}

pub fn day_report(store: &Store, date: NaiveDate, now: i64) -> (i64, i64, Report) {
    let from = local_midnight(date);
    let to = local_midnight(date.succ_opt().expect("date in range")).min(now.max(from));
    // windows opened in the last hour of the previous day still count
    let events = store.events(from - 3600, to);
    let inputs = store.inputs(from - 3600, to);
    (
        from,
        to,
        attribute(&events, &inputs, from, to, &Params::default()),
    )
}

pub fn format_duration(seconds: i64) -> String {
    let (hours, minutes) = (seconds / 3600, seconds % 3600 / 60);
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m"),
        _ => format!("{hours}h {minutes:02}m"),
    }
}

pub fn print_text(date: NaiveDate, report: &Report) {
    println!("{}", date.format("%a %-d %b %Y"));
    println!();
    if report.projects.is_empty() {
        println!("  No coding agent activity recorded.");
        return;
    }
    let width = report.projects.keys().map(String::len).max().unwrap_or(0);
    for (project, totals) in &report.projects {
        let mut tools: Vec<String> = totals.sources.iter().cloned().collect();
        if !totals.models.is_empty() {
            tools.push(format!(
                "({})",
                totals.models.iter().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        println!(
            "  {project:<width$}  {:>8}  {:>3} prompt{}  {}",
            format_duration(totals.credited_seconds),
            totals.prompts,
            if totals.prompts == 1 { " " } else { "s" },
            tools.join(" ")
        );
    }
    println!();
    println!(
        "  AI-assisted time: {}",
        format_duration(report.credited_seconds())
    );
    if report.agent_only_seconds > 0 {
        println!(
            "  Agents worked another {} while you were away (not counted).",
            format_duration(report.agent_only_seconds)
        );
    }
}

/// Machine-readable report. Time is integer seconds everywhere; `slots_utc`
/// uses `{ "YYYY-MM-DD": { "<5-minute slot 0-287>": seconds } }` in UTC.
pub fn to_json(date: NaiveDate, from: i64, to: i64, report: &Report) -> Value {
    let projects: Vec<Value> = report
        .projects
        .iter()
        .map(|(project, totals)| {
            let mut slots: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
            for (start, seconds) in &totals.slots {
                let time = DateTime::from_timestamp(*start, 0).expect("timestamp in range");
                let slot = start.rem_euclid(86_400) / 300; // 86400 seconds per day, 300 per slot
                slots
                    .entry(time.date_naive().to_string())
                    .or_default()
                    .insert(slot.to_string(), *seconds);
            }
            json!({
                "project": project,
                "credited_seconds": totals.credited_seconds,
                "prompts": totals.prompts,
                "sources": totals.sources,
                "models": totals.models,
                "slots_utc": slots,
            })
        })
        .collect();
    json!({
        "date": date.to_string(),
        "from": from,
        "to": to,
        "credited_seconds": report.credited_seconds(),
        "agent_only_seconds": report.agent_only_seconds,
        "projects": projects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations() {
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(420), "7m");
        assert_eq!(format_duration(3600 * 2 + 60 * 5 + 59), "2h 05m");
    }
}
