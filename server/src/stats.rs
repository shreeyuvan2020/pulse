//! Numbers shown on profiles. Days are UTC days, counted from the unix epoch.

use std::collections::BTreeMap;

pub const DAY: i64 = 86_400;

/// A day counts toward a streak once it has this much credited time: 5 minutes.
const STREAK_MIN_SECONDS: i64 = 300;

#[derive(Debug, PartialEq, Eq)]
pub struct Streaks {
    pub current: u32,
    pub longest: u32,
}

/// `days` maps a day number to its credited seconds. The current streak ends
/// today or yesterday, so it does not read 0 every morning before the first
/// session.
pub fn streaks(days: &BTreeMap<i64, i64>, today: i64) -> Streaks {
    let counts = |day: i64| {
        days.get(&day)
            .is_some_and(|seconds| *seconds >= STREAK_MIN_SECONDS)
    };

    let mut current = 0u32;
    let mut day = if counts(today) { today } else { today - 1 };
    while counts(day) {
        current += 1;
        day -= 1;
    }

    let (mut longest, mut run, mut previous) = (0u32, 0u32, None);
    for (day, seconds) in days {
        if *seconds < STREAK_MIN_SECONDS {
            continue;
        }
        run = if previous == Some(day - 1) {
            run + 1
        } else {
            1
        };
        previous = Some(*day);
        longest = longest.max(run);
    }
    Streaks { current, longest }
}

pub fn date_string(day: i64) -> String {
    chrono::DateTime::from_timestamp(day * DAY, 0)
        .expect("day in range")
        .date_naive()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days(list: &[(i64, i64)]) -> BTreeMap<i64, i64> {
        list.iter().copied().collect()
    }

    #[test]
    fn current_streak_may_end_yesterday_but_not_earlier() {
        let history = days(&[(7, 600), (8, 300), (9, 4000)]);
        assert_eq!(streaks(&history, 9).current, 3);
        // nothing yet today: still alive through yesterday
        assert_eq!(streaks(&history, 10).current, 3);
        // two days with nothing: over
        assert_eq!(streaks(&history, 11).current, 0);
    }

    #[test]
    fn days_under_five_minutes_do_not_count_and_longest_is_not_current() {
        // run of 4 (days 1-4), a 299 second day breaks it, then a run of 2
        let history = days(&[
            (1, 300),
            (2, 300),
            (3, 300),
            (4, 300),
            (5, 299),
            (6, 900),
            (7, 900),
        ]);
        assert_eq!(
            streaks(&history, 7),
            Streaks {
                current: 2,
                longest: 4
            }
        );
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(date_string(0), "1970-01-01");
        assert_eq!(date_string(20_411), "2025-11-19");
    }
}
