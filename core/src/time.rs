//! Wall-clock helpers. Business days and stamps use UTC+8.

use chrono::{DateTime, Duration, NaiveTime};

pub const MINUTE: i64 = 60_000;
pub const HOUR: i64 = 60 * MINUTE;
pub const DAY: i64 = 24 * HOUR;

/// Milliseconds since the Unix epoch.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn local(at: i64) -> Option<chrono::NaiveDateTime> {
    DateTime::from_timestamp_millis(at).map(|d| (d + Duration::hours(8)).naive_utc())
}

/// `YYYY-MM-DD` of the UTC+8 day containing `at`.
pub fn date_from(at: i64) -> String {
    local(at).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

pub fn today() -> String {
    date_from(now())
}

/// `YYYY-MM-DD HH:MM:SS` in UTC+8.
pub fn stamp(at: i64) -> String {
    local(at).map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default()
}

/// Minutes since local midnight for `at`.
pub fn minute_of_day(at: i64) -> u32 {
    use chrono::Timelike;
    local(at).map(|d| d.hour() * 60 + d.minute()).unwrap_or(0)
}

/// Epoch milliseconds of local midnight starting the day `date`, which must
/// be exactly `YYYY-MM-DD`.
pub fn day_start(date: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok().filter(|d| d.to_string() == date)?;
    Some(d.and_time(NaiveTime::MIN).and_utc().timestamp_millis() - 8 * HOUR)
}

/// Parse `HH:MM` into minutes since midnight.
pub fn parse_hm(text: &str) -> Option<u32> {
    let t = NaiveTime::parse_from_str(text.trim(), "%H:%M").ok()?;
    use chrono::Timelike;
    Some(t.hour() * 60 + t.minute())
}

#[cfg(test)]
mod tests;
