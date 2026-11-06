//! Work calendar: published holiday-cn years, user corrections and the
//! weekend rule. Precedence:
//!
//! 1. a manual override for the date;
//! 2. an explicit record from a covering publication year (the date's own
//!    year and, for a December date, the next year's file); the later
//!    publication wins, and an absent record never erases an older one;
//! 3. with a covering year, Saturday and Sunday are weekend, other days
//!    workdays;
//! 4. no covering year is `unknown`, never an assumed weekday.
//!
//! A year older than the TTL stays usable and carries a stale diagnostic.
//! Years and overrides live in SQLite (`data::ai`); a [`Calendar`] holds only the rows a
//! single classification needs.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{Error, Result};

pub const MAX_BODY_BYTES: usize = 512 * 1024;
pub const MAX_DAYS: usize = 400;
pub const MAX_PAPERS: usize = 64;
pub const CACHE_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub const MIN_YEAR: i32 = 1900;
pub const MAX_YEAR: i32 = 2100;
const MAX_META_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DayKind {
    Holiday,
    Workday,
}

impl DayKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Holiday => "holiday",
            Self::Workday => "workday",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "holiday" => Some(Self::Holiday),
            "workday" => Some(Self::Workday),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Workday,
    Weekend,
    Holiday,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Manual,
    Cache,
    Weekend,
}

/// One classified Shanghai-local date.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Day {
    pub date: NaiveDate,
    pub kind: Kind,
    pub source: Option<Source>,
    pub publication_year: Option<i32>,
    pub fetched_at: Option<i64>,
    /// `Some(age)` when the deciding year is older than the TTL.
    pub stale_age: Option<i64>,
}

impl Day {
    pub fn is_workday(&self) -> bool {
        self.kind == Kind::Workday
    }

    /// The `calendar_day` object of the `work_state` snapshot.
    pub fn json(&self) -> Value {
        let kind = match self.kind {
            Kind::Workday => "workday",
            Kind::Weekend => "weekend",
            Kind::Holiday => "holiday",
            Kind::Unknown => "unknown",
        };
        let source = self.source.map(|s| match s {
            Source::Manual => "manual",
            Source::Cache => "cache",
            Source::Weekend => "weekend",
        });
        let diagnostics = match (self.kind, self.stale_age) {
            (Kind::Unknown, _) => vec![json!({"kind": "no_coverage"})],
            (_, Some(age)) => vec![json!({
                "kind": "stale_cache",
                "publication_year": self.publication_year,
                "fetched_at_ms": self.fetched_at,
                "age_ms": age,
            })],
            _ => Vec::new(),
        };
        json!({
            "date": self.date.to_string(),
            "kind": kind,
            "source": source,
            "publication_year": self.publication_year,
            "fetched_at_ms": self.fetched_at,
            "stale": self.stale_age.is_some(),
            "diagnostics": diagnostics,
        })
    }
}

/// One validated publication year.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Year {
    pub year: i32,
    pub fetched_at: i64,
    pub days: BTreeMap<NaiveDate, DayKind>,
}

/// The rows needed to classify dates in a small range.
#[derive(Clone, Debug, Default)]
pub struct Calendar {
    years: BTreeMap<i32, Year>,
    overrides: BTreeMap<NaiveDate, DayKind>,
}

impl Calendar {
    pub fn insert_year(&mut self, year: Year) {
        self.years.insert(year.year, year);
    }

    pub fn set_override(&mut self, date: NaiveDate, kind: DayKind) {
        self.overrides.insert(date, kind);
    }

    pub fn classify(&self, date: NaiveDate, at: i64) -> Day {
        if let Some(kind) = self.overrides.get(&date) {
            return Day {
                date,
                kind: match kind {
                    DayKind::Holiday => Kind::Holiday,
                    DayKind::Workday => Kind::Workday,
                },
                source: Some(Source::Manual),
                publication_year: None,
                fetched_at: None,
                stale_age: None,
            };
        }
        let mut coverage = None;
        let mut explicit = None;
        for year in covering_years(date).filter_map(|y| self.years.get(&y)) {
            coverage = Some(year);
            if let Some(kind) = year.days.get(&date) {
                explicit = Some((year, *kind));
            }
        }
        let Some(year) = explicit.map(|(y, _)| y).or(coverage) else {
            return Day {
                date,
                kind: Kind::Unknown,
                source: None,
                publication_year: None,
                fetched_at: None,
                stale_age: None,
            };
        };
        let age = at.saturating_sub(year.fetched_at);
        let (kind, source) = match explicit {
            Some((_, DayKind::Holiday)) => (Kind::Holiday, Source::Cache),
            Some((_, DayKind::Workday)) => (Kind::Workday, Source::Cache),
            None if matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun) => {
                (Kind::Weekend, Source::Weekend)
            }
            None => (Kind::Workday, Source::Cache),
        };
        Day {
            date,
            kind,
            source: Some(source),
            publication_year: Some(year.year),
            fetched_at: Some(year.fetched_at),
            stale_age: (age >= CACHE_TTL_MS).then_some(age),
        }
    }
}

/// A date is covered by its own year's file and, in December, by the next
/// year's file, which publishes the New Year arrangement.
fn covering_years(date: NaiveDate) -> std::ops::RangeInclusive<i32> {
    let next = date.month() == 12 && date.year() < MAX_YEAR;
    date.year()..=date.year() + i32::from(next)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawYear {
    #[serde(rename = "$schema", default)]
    schema: Option<String>,
    #[serde(rename = "$id", default)]
    id: Option<String>,
    year: i32,
    #[serde(default)]
    papers: Option<Vec<String>>,
    days: Vec<RawDay>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDay {
    #[serde(default, rename = "name")]
    _name: Option<String>,
    date: String,
    #[serde(rename = "isOffDay")]
    is_off_day: bool,
}

/// Parse one upstream holiday-cn file strictly: the known fields only,
/// dates in `year` or its previous December, no duplicates, bounded sizes.
/// The reason is diagnostic only; callers report the fetch as failed.
pub fn parse_year(bytes: &[u8], fetched_at: i64) -> std::result::Result<Year, String> {
    if bytes.is_empty() {
        return Err("holiday data is empty".into());
    }
    if bytes.len() > MAX_BODY_BYTES {
        return Err(format!("holiday data is too large ({} bytes, max {MAX_BODY_BYTES})", bytes.len()));
    }
    let raw: RawYear = serde_json::from_slice(bytes).map_err(|e| format!("holiday JSON is invalid: {e}"))?;
    for (name, meta) in [("$schema", &raw.schema), ("$id", &raw.id)] {
        if meta.as_ref().is_some_and(|m| !valid_meta(m)) {
            return Err(format!("holiday upstream metadata {name} is invalid"));
        }
    }
    if !(MIN_YEAR..=MAX_YEAR).contains(&raw.year) {
        return Err(format!("holiday year {} is outside {MIN_YEAR}..={MAX_YEAR}", raw.year));
    }
    if raw.days.is_empty() {
        return Err("holiday year has no days".into());
    }
    if raw.days.len() > MAX_DAYS {
        return Err(format!("holiday year has too many days ({})", raw.days.len()));
    }
    if raw.papers.as_ref().is_some_and(|p| p.len() > MAX_PAPERS) {
        return Err(format!("holiday year has too many source papers ({})", raw.papers.map_or(0, |p| p.len())));
    }
    let mut days = BTreeMap::new();
    for day in raw.days {
        let date = parse_iso_date(&day.date).ok_or_else(|| format!("invalid holiday date {:?}", day.date))?;
        let in_year = date.year() == raw.year || (date.year() == raw.year - 1 && date.month() == 12);
        if !in_year {
            return Err(format!(
                "date {date} is outside publication year {} (only that year and its previous December are allowed)",
                raw.year
            ));
        }
        let kind = if day.is_off_day { DayKind::Holiday } else { DayKind::Workday };
        if days.insert(date, kind).is_some() {
            return Err(format!("contradictory or duplicate date {date}"));
        }
    }
    Ok(Year { year: raw.year, fetched_at, days })
}

fn valid_meta(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && value.chars().count() <= MAX_META_CHARS
        && !value.chars().any(char::is_control)
}

/// Strict `YYYY-MM-DD` inside the supported years.
pub fn parse_iso_date(raw: &str) -> Option<NaiveDate> {
    let b = raw.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' || !digits(0..4) || !digits(5..7) || !digits(8..10) {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(raw[..4].parse().ok()?, raw[5..7].parse().ok()?, raw[8..].parse().ok()?)?;
    (MIN_YEAR..=MAX_YEAR).contains(&date.year()).then_some(date)
}

const MIRRORS: [&str; 2] = [
    "https://raw.githubusercontent.com/NateScarlet/holiday-cn/master",
    "https://cdn.jsdelivr.net/gh/NateScarlet/holiday-cn@master",
];

/// Years the calendar should hold at `at`: this year, and next year in
/// December.
pub fn wanted_years(today: NaiveDate) -> Vec<i32> {
    if today.month() == 12 {
        vec![today.year(), today.year() + 1]
    } else {
        vec![today.year()]
    }
}

/// Download one year from the mirrors in order. `get` performs a bounded
/// GET (15 s, 512 KiB, two redirects) and returns the 200 body.
pub async fn fetch_year<F, Fut>(year: i32, at: i64, get: F) -> Result<Year>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<Vec<u8>, String>>,
{
    for base in MIRRORS {
        if let Ok(body) = get(format!("{base}/{year}.json")).await {
            if let Ok(parsed) = parse_year(&body, at) {
                if parsed.year == year {
                    return Ok(parsed);
                }
            }
        }
    }
    Err(Error(format!("{year} 年日历获取失败；已有缓存继续使用，无缓存时请确认日期或开启加班")))
}
