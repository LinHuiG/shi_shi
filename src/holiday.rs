//! Strict holiday-cn calendar parsing, versioned cache data, manual overrides
//! and local weekday inference.
//!
//! This module never fetches data. A caller supplies raw upstream bytes and
//! cache metadata. All timestamps accepted or returned here are Unix
//! **milliseconds**, the unit used by the application's `model::now()`. There is
//! deliberately no seconds-based API in this crate; convert at an external
//! boundary only if a source supplies seconds. Merge precedence is deliberately
//! explicit:
//!
//! 1. manual per-day override;
//! 2. for an explicit published record, the record from the cache with the
//!    greatest publication year wins (a file `year=Y` may contain December
//!    `Y-1` records and therefore overrides the older `Y-1` file for that
//!    date);
//! 3. absent an explicit record, only a cache that covers the date permits
//!    local inference: Saturday/Sunday become weekend, other days a workday;
//! 4. no covering cache means `Unknown`, never an assumed weekday.
//!
//! A newer cache with no record for a December date does not delete an older
//! cache's explicit record for that date. Only records conflict; sparse
//! absence is not treated as a deletion.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

/// Maximum accepted upstream or cache JSON body.
pub const MAX_BODY_BYTES: usize = 512 * 1024;
/// One published year has at most ~397 relevant dates.
pub const MAX_DAYS: usize = 400;
/// Hard cap for the unused `papers` array in the upstream file.
pub const MAX_PAPERS: usize = 64;
pub const CACHE_SCHEMA_VERSION: u32 = 1;
/// Seven days in Unix milliseconds, matching the application timestamp unit.
pub const CACHE_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
pub const MIN_YEAR: i32 = 1900;
pub const MAX_YEAR: i32 = 2100;
pub const MAX_SOURCE_CHARS: usize = 512;
pub const MAX_ETAG_CHARS: usize = 512;
pub const MAX_LAST_MODIFIED_CHARS: usize = 128;
pub const MAX_NOTE_CHARS: usize = 512;
/// Bound for the upstream JSON Schema annotation strings (`$schema`, `$id`).
pub const MAX_UPSTREAM_META_CHARS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolidayError {
    BodyTooLarge { len: usize, max: usize },
    EmptyBody,
    InvalidJson(String),
    UnsupportedSchemaVersion { found: u32, expected: u32 },
    InvalidYear(i32),
    EmptyDays,
    TooManyDays(usize),
    TooManyPapers(usize),
    InvalidDate(String),
    DateOutsidePublicationYear { date: NaiveDate, year: i32 },
    DuplicateDate(NaiveDate),
    CacheDaysNotStrictlyIncreasing(NaiveDate),
    InvalidSource,
    InvalidHeader(&'static str),
    InvalidFetchedAtMs(i64),
    InvalidUpstreamMetadata(&'static str),
    InvalidManualOverride(&'static str),
}

impl fmt::Display for HolidayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BodyTooLarge { len, max } => {
                write!(f, "holiday data is too large ({len} bytes, max {max})")
            }
            Self::EmptyBody => write!(f, "holiday data is empty"),
            Self::InvalidJson(detail) => write!(f, "holiday JSON is invalid: {detail}"),
            Self::UnsupportedSchemaVersion { found, expected } => write!(
                f,
                "unsupported holiday cache schema version {found} (expected {expected})"
            ),
            Self::InvalidYear(year) => {
                write!(f, "holiday year {year} is outside {MIN_YEAR}..={MAX_YEAR}")
            }
            Self::EmptyDays => write!(f, "holiday year has no days"),
            Self::TooManyDays(count) => write!(f, "holiday year has too many days ({count})"),
            Self::TooManyPapers(count) => {
                write!(f, "holiday year has too many source papers ({count})")
            }
            Self::InvalidDate(raw) => write!(f, "invalid holiday date {raw:?}"),
            Self::DateOutsidePublicationYear { date, year } => write!(
                f,
                "date {date} is outside publication year {year} (only that year and its previous December are allowed)"
            ),
            Self::DuplicateDate(date) => write!(f, "contradictory or duplicate date {date}"),
            Self::CacheDaysNotStrictlyIncreasing(date) => write!(
                f,
                "holiday cache days are not strictly increasing at {date}"
            ),
            Self::InvalidSource => write!(f, "holiday cache source is empty or invalid"),
            Self::InvalidHeader(name) => write!(f, "holiday cache {name} header is invalid"),
            Self::InvalidFetchedAtMs(ts) => {
                write!(f, "holiday cache fetched_at_ms {ts} is invalid")
            }
            Self::InvalidUpstreamMetadata(field) => {
                write!(f, "holiday upstream metadata {field} is invalid")
            }
            Self::InvalidManualOverride(detail) => {
                write!(f, "manual day override is invalid: {detail}")
            }
        }
    }
}

impl std::error::Error for HolidayError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DayKind {
    Holiday,
    Workday,
}

impl DayKind {
    pub fn is_holiday(self) -> bool {
        matches!(self, Self::Holiday)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarKind {
    Workday,
    Weekend,
    Holiday,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CalendarSource {
    Manual,
    Cache,
    Weekend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CalendarDiagnostic {
    NoCoverage,
    StaleCache {
        publication_year: i32,
        fetched_at_ms: i64,
        age_ms: i64,
    },
}

/// Result of classifying one Shanghai-local calendar date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarDay {
    pub date: NaiveDate,
    pub kind: CalendarKind,
    pub source: Option<CalendarSource>,
    pub publication_year: Option<i32>,
    pub fetched_at_ms: Option<i64>,
    pub stale: bool,
    pub diagnostics: Vec<CalendarDiagnostic>,
}

impl CalendarDay {
    pub fn is_workday(&self) -> bool {
        self.kind == CalendarKind::Workday
    }
}

/// A user-entered day correction. It wins over every published/weekend source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualOverride {
    pub date: NaiveDate,
    pub kind: DayKind,
    pub updated_at_ms: i64,
    pub note: Option<String>,
}

impl ManualOverride {
    pub fn new(
        date: NaiveDate,
        kind: DayKind,
        updated_at_ms: i64,
        note: Option<String>,
    ) -> Result<Self, HolidayError> {
        let value = Self {
            date,
            kind,
            updated_at_ms,
            note,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), HolidayError> {
        if !date_in_supported_range(self.date) {
            return Err(HolidayError::InvalidManualOverride("date is out of range"));
        }
        if self.updated_at_ms <= 0 {
            return Err(HolidayError::InvalidManualOverride(
                "updated_at_ms is not positive",
            ));
        }
        if let Some(note) = &self.note {
            if note.chars().count() > MAX_NOTE_CHARS || note.chars().any(|c| c.is_control()) {
                return Err(HolidayError::InvalidManualOverride("note is invalid"));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CachedDay {
    pub date: NaiveDate,
    pub kind: DayKind,
}

/// Versioned on-disk representation of one published year.
///
/// `year` is the publication (upstream file) year. `days` may include dates in
/// December of `year - 1` because that is how holiday-cn publishes the next
/// New Year arrangement. No migration or default-filled fields are supported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HolidayCache {
    pub schema_version: u32,
    pub year: i32,
    pub source: String,
    pub fetched_at_ms: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub days: Vec<CachedDay>,
}

impl HolidayCache {
    pub fn from_published(
        published: &PublishedHolidayYear,
        source: impl Into<String>,
        fetched_at_ms: i64,
        etag: Option<String>,
        last_modified: Option<String>,
    ) -> Result<Self, HolidayError> {
        let cache = Self {
            schema_version: CACHE_SCHEMA_VERSION,
            year: published.year,
            source: source.into(),
            fetched_at_ms,
            etag,
            last_modified,
            days: published
                .days
                .iter()
                .map(|(date, kind)| CachedDay {
                    date: *date,
                    kind: *kind,
                })
                .collect(),
        };
        cache.validate()?;
        Ok(cache)
    }

    pub fn validate(&self) -> Result<(), HolidayError> {
        if self.schema_version != CACHE_SCHEMA_VERSION {
            return Err(HolidayError::UnsupportedSchemaVersion {
                found: self.schema_version,
                expected: CACHE_SCHEMA_VERSION,
            });
        }
        validate_year(self.year)?;
        validate_text(&self.source, MAX_SOURCE_CHARS).map_err(|_| HolidayError::InvalidSource)?;
        if self.fetched_at_ms <= 0 {
            return Err(HolidayError::InvalidFetchedAtMs(self.fetched_at_ms));
        }
        if let Some(etag) = &self.etag {
            validate_text(etag, MAX_ETAG_CHARS).map_err(|_| HolidayError::InvalidHeader("etag"))?;
        }
        if let Some(last_modified) = &self.last_modified {
            validate_text(last_modified, MAX_LAST_MODIFIED_CHARS)
                .map_err(|_| HolidayError::InvalidHeader("last_modified"))?;
        }
        if self.days.is_empty() {
            return Err(HolidayError::EmptyDays);
        }
        if self.days.len() > MAX_DAYS {
            return Err(HolidayError::TooManyDays(self.days.len()));
        }
        let mut previous = None;
        for day in &self.days {
            validate_publication_date(day.date, self.year)?;
            if let Some(prev) = previous {
                if day.date <= prev {
                    return Err(HolidayError::CacheDaysNotStrictlyIncreasing(day.date));
                }
            }
            previous = Some(day.date);
        }
        Ok(())
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, HolidayError> {
        check_body(bytes)?;
        let cache: Self =
            serde_json::from_slice(bytes).map_err(|e| HolidayError::InvalidJson(e.to_string()))?;
        cache.validate()?;
        Ok(cache)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, HolidayError> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|e| HolidayError::InvalidJson(e.to_string()))
    }

    pub fn is_stale(&self, now_ms: i64) -> bool {
        now_ms.saturating_sub(self.fetched_at_ms) >= CACHE_TTL_MS
    }

    pub fn days_map(&self) -> Result<BTreeMap<NaiveDate, DayKind>, HolidayError> {
        self.validate()?;
        Ok(self.days.iter().map(|d| (d.date, d.kind)).collect())
    }
}

/// Validated upstream data. Kept separate from the timestamped cache so the
/// parser has no notion of freshness or HTTP metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedHolidayYear {
    pub year: i32,
    pub days: BTreeMap<NaiveDate, DayKind>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawYear {
    /// JSON Schema annotation used by the selected upstream files. It is
    /// validated but deliberately not persisted here.
    #[serde(rename = "$schema", default)]
    schema: Option<String>,
    /// JSON Schema `$id` annotation from the selected upstream files.
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

/// Parse only the selected holiday-cn `year`/`days` fields from upstream bytes.
///
/// The upstream JSON Schema annotations `$schema` and `$id` are accepted as
/// optional bounded strings. The known unused `papers` and per-day `name`
/// fields are accepted, all other root/day fields are rejected. Records must
/// belong to `year` or December of `year-1`. The versioned [`HolidayCache`]
/// stays strict and does not carry these upstream annotations.
pub fn parse_holiday_year(bytes: &[u8]) -> Result<PublishedHolidayYear, HolidayError> {
    check_body(bytes)?;
    let raw: RawYear =
        serde_json::from_slice(bytes).map_err(|e| HolidayError::InvalidJson(e.to_string()))?;
    if let Some(schema) = &raw.schema {
        validate_upstream_metadata("$schema", schema)?;
    }
    if let Some(id) = &raw.id {
        validate_upstream_metadata("$id", id)?;
    }
    validate_year(raw.year)?;
    if raw.days.is_empty() {
        return Err(HolidayError::EmptyDays);
    }
    if raw.days.len() > MAX_DAYS {
        return Err(HolidayError::TooManyDays(raw.days.len()));
    }
    if let Some(papers) = &raw.papers {
        if papers.len() > MAX_PAPERS {
            return Err(HolidayError::TooManyPapers(papers.len()));
        }
    }

    let mut days = BTreeMap::new();
    for raw_day in raw.days {
        let date = parse_iso_date(&raw_day.date)?;
        validate_publication_date(date, raw.year)?;
        let kind = if raw_day.is_off_day {
            DayKind::Holiday
        } else {
            DayKind::Workday
        };
        if days.insert(date, kind).is_some() {
            return Err(HolidayError::DuplicateDate(date));
        }
    }
    Ok(PublishedHolidayYear {
        year: raw.year,
        days,
    })
}

/// In-memory calendar source set for one or more publication years.
#[derive(Debug, Clone, Default)]
pub struct CalendarData {
    caches: BTreeMap<i32, HolidayCache>,
    manual_overrides: BTreeMap<NaiveDate, ManualOverride>,
}

impl CalendarData {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert/replace the cache for one publication year. A cache with a
    /// conflicting day inside its own JSON is already rejected by its parser.
    pub fn insert_cache(&mut self, cache: HolidayCache) -> Result<(), HolidayError> {
        cache.validate()?;
        self.caches.insert(cache.year, cache);
        Ok(())
    }

    pub fn remove_cache(&mut self, year: i32) -> Option<HolidayCache> {
        self.caches.remove(&year)
    }

    pub fn cache(&self, year: i32) -> Option<&HolidayCache> {
        self.caches.get(&year)
    }

    pub fn caches(&self) -> impl Iterator<Item = &HolidayCache> {
        self.caches.values()
    }

    pub fn set_manual_override(&mut self, value: ManualOverride) -> Result<(), HolidayError> {
        value.validate()?;
        self.manual_overrides.insert(value.date, value);
        Ok(())
    }

    pub fn remove_manual_override(&mut self, date: NaiveDate) -> Option<ManualOverride> {
        self.manual_overrides.remove(&date)
    }

    pub fn manual_override(&self, date: NaiveDate) -> Option<&ManualOverride> {
        self.manual_overrides.get(&date)
    }

    /// Classify one Shanghai calendar date at an explicit lookup time in Unix
    /// milliseconds.
    ///
    /// The lookup time only controls the stale diagnostic. A stale cache is
    /// still authoritative and usable; a missing covering cache is `Unknown`.
    pub fn classify(&self, date: NaiveDate, now_ms: i64) -> CalendarDay {
        if let Some(manual) = self.manual_overrides.get(&date) {
            return CalendarDay {
                date,
                kind: match manual.kind {
                    DayKind::Holiday => CalendarKind::Holiday,
                    DayKind::Workday => CalendarKind::Workday,
                },
                source: Some(CalendarSource::Manual),
                publication_year: None,
                fetched_at_ms: None,
                stale: false,
                diagnostics: Vec::new(),
            };
        }

        let years = applicable_publication_years(date);
        let mut explicit: Option<(&HolidayCache, DayKind)> = None;
        for year in &years {
            if let Some(cache) = self.caches.get(year) {
                if let Some(kind) = cache.days.iter().find(|d| d.date == date).map(|d| d.kind) {
                    // Caches are visited in ascending publication-year order.
                    explicit = Some((cache, kind));
                }
            }
        }
        if let Some((cache, kind)) = explicit {
            let stale = cache.is_stale(now_ms);
            let mut diagnostics = Vec::new();
            if stale {
                diagnostics.push(stale_diagnostic(cache, now_ms));
            }
            return CalendarDay {
                date,
                kind: match kind {
                    DayKind::Holiday => CalendarKind::Holiday,
                    DayKind::Workday => CalendarKind::Workday,
                },
                source: Some(CalendarSource::Cache),
                publication_year: Some(cache.year),
                fetched_at_ms: Some(cache.fetched_at_ms),
                stale,
                diagnostics,
            };
        }

        let coverage = years
            .iter()
            .filter_map(|year| self.caches.get(year))
            .max_by_key(|cache| cache.year);
        let Some(cache) = coverage else {
            return CalendarDay {
                date,
                kind: CalendarKind::Unknown,
                source: None,
                publication_year: None,
                fetched_at_ms: None,
                stale: false,
                diagnostics: vec![CalendarDiagnostic::NoCoverage],
            };
        };

        let stale = cache.is_stale(now_ms);
        let mut diagnostics = Vec::new();
        if stale {
            diagnostics.push(stale_diagnostic(cache, now_ms));
        }
        let weekend = matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun);
        CalendarDay {
            date,
            kind: if weekend {
                CalendarKind::Weekend
            } else {
                CalendarKind::Workday
            },
            source: Some(if weekend {
                CalendarSource::Weekend
            } else {
                CalendarSource::Cache
            }),
            publication_year: Some(cache.year),
            fetched_at_ms: Some(cache.fetched_at_ms),
            stale,
            diagnostics,
        }
    }
}

fn stale_diagnostic(cache: &HolidayCache, now_ms: i64) -> CalendarDiagnostic {
    CalendarDiagnostic::StaleCache {
        publication_year: cache.year,
        fetched_at_ms: cache.fetched_at_ms,
        age_ms: now_ms.saturating_sub(cache.fetched_at_ms),
    }
}

/// A date is covered by its own year's file and, in December, the next year's
/// file (which can publish the following New Year arrangement).
fn applicable_publication_years(date: NaiveDate) -> Vec<i32> {
    let mut years = vec![date.year()];
    if date.month() == 12 && date.year() < MAX_YEAR {
        years.push(date.year() + 1);
    }
    years
}

fn check_body(bytes: &[u8]) -> Result<(), HolidayError> {
    if bytes.is_empty() {
        return Err(HolidayError::EmptyBody);
    }
    if bytes.len() > MAX_BODY_BYTES {
        return Err(HolidayError::BodyTooLarge {
            len: bytes.len(),
            max: MAX_BODY_BYTES,
        });
    }
    Ok(())
}

fn validate_year(year: i32) -> Result<(), HolidayError> {
    if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
        return Err(HolidayError::InvalidYear(year));
    }
    Ok(())
}

fn date_in_supported_range(date: NaiveDate) -> bool {
    (MIN_YEAR..=MAX_YEAR).contains(&date.year())
}

fn validate_publication_date(date: NaiveDate, year: i32) -> Result<(), HolidayError> {
    if !date_in_supported_range(date) {
        return Err(HolidayError::DateOutsidePublicationYear { date, year });
    }
    let same_year = date.year() == year;
    let previous_december = date.year() == year - 1 && date.month() == 12;
    if same_year || previous_december {
        Ok(())
    } else {
        Err(HolidayError::DateOutsidePublicationYear { date, year })
    }
}

fn parse_iso_date(raw: &str) -> Result<NaiveDate, HolidayError> {
    let bytes = raw.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
    {
        return Err(HolidayError::InvalidDate(raw.to_owned()));
    }
    let year: i32 = raw[..4]
        .parse()
        .map_err(|_| HolidayError::InvalidDate(raw.to_owned()))?;
    let month: u32 = raw[5..7]
        .parse()
        .map_err(|_| HolidayError::InvalidDate(raw.to_owned()))?;
    let day: u32 = raw[8..]
        .parse()
        .map_err(|_| HolidayError::InvalidDate(raw.to_owned()))?;
    if !(MIN_YEAR..=MAX_YEAR).contains(&year) {
        return Err(HolidayError::InvalidDate(raw.to_owned()));
    }
    NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| HolidayError::InvalidDate(raw.to_owned()))
}

fn validate_text(value: &str, max_chars: usize) -> Result<(), ()> {
    if value.is_empty() || value.trim() != value || value.chars().count() > max_chars {
        return Err(());
    }
    if value.chars().any(char::is_control) {
        return Err(());
    }
    Ok(())
}

fn validate_upstream_metadata(field: &'static str, value: &str) -> Result<(), HolidayError> {
    validate_text(value, MAX_UPSTREAM_META_CHARS)
        .map_err(|_| HolidayError::InvalidUpstreamMetadata(field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use serde_json::json;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn ts_ms(y: i32, m: u32, day: u32, h: u32, min: u32) -> i64 {
        d(y, m, day)
            .and_hms_opt(h, min, 0)
            .unwrap()
            .and_utc()
            .timestamp_millis()
            - 8 * 3_600_000
    }

    fn cache(year: i32, fetched_at_ms: i64, days: &[(NaiveDate, DayKind)]) -> HolidayCache {
        let published = PublishedHolidayYear {
            year,
            days: days.iter().copied().collect(),
        };
        HolidayCache::from_published(
            &published,
            format!("synthetic://holiday/{year}"),
            fetched_at_ms,
            Some(format!("etag-{year}")),
            Some("Wed, 01 Jan 2025 00:00:00 GMT".to_owned()),
        )
        .unwrap()
    }

    fn calendar_2025(fetched_at_ms: i64) -> CalendarData {
        let mut calendar = CalendarData::new();
        calendar
            .insert_cache(cache(
                2025,
                fetched_at_ms,
                &[
                    (d(2025, 1, 1), DayKind::Holiday),
                    (d(2025, 1, 11), DayKind::Workday),
                ],
            ))
            .unwrap();
        calendar
    }

    #[test]
    fn parses_synthetic_upstream_and_accepts_previous_december() {
        let body = br#"{
            "year": 2025,
            "papers": ["https://example.invalid/notice"],
            "days": [
                {"name": "New Year", "date": "2024-12-31", "isOffDay": false},
                {"name": "New Year", "date": "2025-01-01", "isOffDay": true},
                {"name": "Make-up", "date": "2025-01-11", "isOffDay": false}
            ]
        }"#;
        let parsed = parse_holiday_year(body).unwrap();
        assert_eq!(parsed.year, 2025);
        assert_eq!(parsed.days.len(), 3);
        assert_eq!(parsed.days[&d(2024, 12, 31)], DayKind::Workday);
        assert_eq!(parsed.days[&d(2025, 1, 1)], DayKind::Holiday);
    }

    #[test]
    fn parses_upstream_json_schema_annotations_with_synthetic_same_shape() {
        // Same root/day shape as the selected holiday-cn files, but synthetic
        // dates and URLs. No annual calendar data is copied into the crate.
        let body = br#"{
            "$schema": "https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/schema.json",
            "$id": "https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/2025.json",
            "year": 2025,
            "papers": ["https://example.invalid/synthetic-notice"],
            "days": [
                {"name": "Synthetic New Year", "date": "2024-12-31", "isOffDay": false},
                {"name": "Synthetic New Year", "date": "2025-01-01", "isOffDay": true},
                {"name": "Synthetic Make-up", "date": "2025-01-11", "isOffDay": false}
            ]
        }"#;
        let parsed = parse_holiday_year(body).unwrap();
        assert_eq!(parsed.year, 2025);
        assert_eq!(parsed.days.len(), 3);
        assert_eq!(parsed.days[&d(2024, 12, 31)], DayKind::Workday);
        assert_eq!(parsed.days[&d(2025, 1, 1)], DayKind::Holiday);
        assert_eq!(parsed.days[&d(2025, 1, 11)], DayKind::Workday);

        let wrong_type =
            br#"{"$schema":42,"year":2025,"days":[{"date":"2025-01-01","isOffDay":true}]}"#;
        assert!(matches!(
            parse_holiday_year(wrong_type),
            Err(HolidayError::InvalidJson(_))
        ));

        let oversized_metadata = json!({
            "$id": "x".repeat(MAX_UPSTREAM_META_CHARS + 1),
            "year": 2025,
            "days": [{"date": "2025-01-01", "isOffDay": true}],
        });
        assert!(matches!(
            parse_holiday_year(&serde_json::to_vec(&oversized_metadata).unwrap()),
            Err(HolidayError::InvalidUpstreamMetadata("$id"))
        ));

        let control_metadata = json!({
            "$schema": "synthetic\u{0000}schema",
            "year": 2025,
            "days": [{"date": "2025-01-01", "isOffDay": true}],
        });
        assert!(matches!(
            parse_holiday_year(&serde_json::to_vec(&control_metadata).unwrap()),
            Err(HolidayError::InvalidUpstreamMetadata("$schema"))
        ));
    }

    #[test]
    fn parser_rejects_bad_shape_fields_years_and_contradictions() {
        let unknown_root = br#"{"year":2025,"days":[],"extra":true}"#;
        assert!(matches!(
            parse_holiday_year(unknown_root),
            Err(HolidayError::InvalidJson(_))
        ));

        let unknown_day = br#"{"year":2025,"days":[{"date":"2025-01-01","isOffDay":true,"x":1}]}"#;
        assert!(matches!(
            parse_holiday_year(unknown_day),
            Err(HolidayError::InvalidJson(_))
        ));

        let missing_is_off_day = br#"{"year":2025,"days":[{"date":"2025-01-01"}]}"#;
        assert!(matches!(
            parse_holiday_year(missing_is_off_day),
            Err(HolidayError::InvalidJson(_))
        ));

        let wrong_is_off_day = br#"{"year":2025,"days":[{"date":"2025-01-01","isOffDay":"yes"}]}"#;
        assert!(matches!(
            parse_holiday_year(wrong_is_off_day),
            Err(HolidayError::InvalidJson(_))
        ));

        let invalid_date = br#"{"year":2025,"days":[{"date":"2025-02-30","isOffDay":true}]}"#;
        assert!(matches!(
            parse_holiday_year(invalid_date),
            Err(HolidayError::InvalidDate(_))
        ));

        let bad_year = br#"{"year":1899,"days":[]}"#;
        assert!(matches!(
            parse_holiday_year(bad_year),
            Err(HolidayError::InvalidYear(1899))
        ));

        let outside = br#"{"year":2025,"days":[{"date":"2024-11-30","isOffDay":true}]}"#;
        assert!(matches!(
            parse_holiday_year(outside),
            Err(HolidayError::DateOutsidePublicationYear { .. })
        ));

        let duplicate = br#"{"year":2025,"days":[
            {"date":"2025-01-01","isOffDay":true},
            {"date":"2025-01-01","isOffDay":false}
        ]}"#;
        assert!(matches!(
            parse_holiday_year(duplicate),
            Err(HolidayError::DuplicateDate(date)) if date == d(2025, 1, 1)
        ));
    }

    #[test]
    fn parser_enforces_size_entry_and_empty_limits() {
        let oversized = vec![b' '; MAX_BODY_BYTES + 1];
        assert!(matches!(
            parse_holiday_year(&oversized),
            Err(HolidayError::BodyTooLarge { .. })
        ));
        assert!(matches!(
            parse_holiday_year(b"  "),
            Err(HolidayError::InvalidJson(_))
        ));
        let empty_days = br#"{"year":2025,"days":[]}"#;
        assert!(matches!(
            parse_holiday_year(empty_days),
            Err(HolidayError::EmptyDays)
        ));

        let raw = json!({
            "year": 2025,
            "days": (0..=MAX_DAYS)
                .map(|_| json!({"date":"2025-01-01","isOffDay":true}))
                .collect::<Vec<_>>(),
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        assert!(matches!(
            parse_holiday_year(&bytes),
            Err(HolidayError::TooManyDays(_))
        ));
    }

    #[test]
    fn versioned_cache_roundtrip_and_strict_schema() {
        let parsed = parse_holiday_year(
            br#"{"year":2025,"days":[
                {"date":"2024-12-31","isOffDay":false},
                {"date":"2025-01-01","isOffDay":true}
            ]}"#,
        )
        .unwrap();
        let cached = HolidayCache::from_published(
            &parsed,
            "synthetic://calendar/2025",
            1_735_689_600_000,
            Some("\"abc\"".to_owned()),
            Some("Wed, 01 Jan 2025 00:00:00 GMT".to_owned()),
        )
        .unwrap();
        let bytes = cached.to_bytes().unwrap();
        let decoded = HolidayCache::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, cached);
        assert_eq!(decoded.schema_version, CACHE_SCHEMA_VERSION);
        assert_eq!(decoded.source, "synthetic://calendar/2025");
        assert_eq!(decoded.etag.as_deref(), Some("\"abc\""));

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["schema_version"] = json!(2);
        let unsupported = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            HolidayCache::from_bytes(&unsupported),
            Err(HolidayError::UnsupportedSchemaVersion { found: 2, .. })
        ));

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value.as_object_mut().unwrap().remove("source");
        let missing = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            HolidayCache::from_bytes(&missing),
            Err(HolidayError::InvalidJson(_))
        ));

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["$schema"] = json!("https://example.invalid/schema.json");
        value["$id"] = json!("https://example.invalid/2025.json");
        let upstream_metadata = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            HolidayCache::from_bytes(&upstream_metadata),
            Err(HolidayError::InvalidJson(_))
        ));

        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["unknown"] = json!(true);
        let unknown = serde_json::to_vec(&value).unwrap();
        assert!(matches!(
            HolidayCache::from_bytes(&unknown),
            Err(HolidayError::InvalidJson(_))
        ));
    }

    #[test]
    fn cache_validation_rejects_unsorted_out_of_range_and_empty_metadata() {
        let mut cached = cache(
            2025,
            ts_ms(2025, 1, 2, 0, 0),
            &[(d(2025, 1, 1), DayKind::Holiday)],
        );
        cached.days.push(CachedDay {
            date: d(2025, 1, 1),
            kind: DayKind::Workday,
        });
        assert!(matches!(
            cached.validate(),
            Err(HolidayError::CacheDaysNotStrictlyIncreasing(_))
        ));

        let mut cached = cache(
            2025,
            ts_ms(2025, 1, 2, 0, 0),
            &[(d(2025, 1, 1), DayKind::Holiday)],
        );
        cached.days[0].date = d(2026, 1, 1);
        assert!(matches!(
            cached.validate(),
            Err(HolidayError::DateOutsidePublicationYear { .. })
        ));

        let mut cached = cache(
            2025,
            ts_ms(2025, 1, 2, 0, 0),
            &[(d(2025, 1, 1), DayKind::Holiday)],
        );
        cached.source = String::new();
        assert!(matches!(
            cached.validate(),
            Err(HolidayError::InvalidSource)
        ));

        let mut cached = cache(
            2025,
            ts_ms(2025, 1, 2, 0, 0),
            &[(d(2025, 1, 1), DayKind::Holiday)],
        );
        cached.fetched_at_ms = 0;
        assert!(matches!(
            cached.validate(),
            Err(HolidayError::InvalidFetchedAtMs(0))
        ));
    }

    #[test]
    fn unknown_is_not_weekday_and_coverage_allows_weekend_inference() {
        let empty = CalendarData::new();
        let unknown = empty.classify(d(2025, 1, 4), ts_ms(2025, 1, 4, 12, 0));
        assert_eq!(unknown.kind, CalendarKind::Unknown);
        assert!(unknown.source.is_none());
        assert!(unknown
            .diagnostics
            .contains(&CalendarDiagnostic::NoCoverage));
        let unknown_weekday = empty.classify(d(2025, 1, 2), ts_ms(2025, 1, 2, 12, 0));
        assert_eq!(unknown_weekday.kind, CalendarKind::Unknown);

        let calendar = calendar_2025(ts_ms(2025, 1, 2, 1, 0));
        let saturday = calendar.classify(d(2025, 1, 4), ts_ms(2025, 1, 4, 12, 0));
        assert_eq!(saturday.kind, CalendarKind::Weekend);
        assert_eq!(saturday.source, Some(CalendarSource::Weekend));
        assert!(!saturday.stale);

        let thursday = calendar.classify(d(2025, 1, 2), ts_ms(2025, 1, 2, 12, 0));
        assert_eq!(thursday.kind, CalendarKind::Workday);
        assert_eq!(thursday.source, Some(CalendarSource::Cache));

        let holiday = calendar.classify(d(2025, 1, 1), ts_ms(2025, 1, 1, 12, 0));
        assert_eq!(holiday.kind, CalendarKind::Holiday);
        let makeup = calendar.classify(d(2025, 1, 11), ts_ms(2025, 1, 11, 12, 0));
        assert_eq!(makeup.kind, CalendarKind::Workday);
    }

    #[test]
    fn manual_override_has_highest_precedence_and_can_confirm_unknown() {
        let mut calendar = calendar_2025(ts_ms(2025, 1, 2, 1, 0));
        calendar
            .set_manual_override(
                ManualOverride::new(
                    d(2025, 1, 4),
                    DayKind::Workday,
                    ts_ms(2025, 1, 2, 1, 0),
                    Some("synthetic manual workday".to_owned()),
                )
                .unwrap(),
            )
            .unwrap();
        let manual = calendar.classify(d(2025, 1, 4), ts_ms(2025, 1, 4, 12, 0));
        assert_eq!(manual.kind, CalendarKind::Workday);
        assert_eq!(manual.source, Some(CalendarSource::Manual));
        assert!(manual.publication_year.is_none());

        let mut override_holiday = CalendarData::new();
        override_holiday
            .set_manual_override(
                ManualOverride::new(
                    d(2025, 1, 2),
                    DayKind::Holiday,
                    ts_ms(2025, 1, 2, 1, 0),
                    None,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            override_holiday
                .classify(d(2025, 1, 2), ts_ms(2025, 1, 2, 12, 0))
                .kind,
            CalendarKind::Holiday
        );
        assert!(override_holiday
            .remove_manual_override(d(2025, 1, 2))
            .is_some());
        assert_eq!(
            override_holiday
                .classify(d(2025, 1, 2), ts_ms(2025, 1, 2, 12, 0))
                .kind,
            CalendarKind::Unknown
        );
    }

    #[test]
    fn stale_cache_remains_usable_with_diagnostic() {
        let fetched_ms = ts_ms(2025, 1, 1, 0, 0);
        let calendar = calendar_2025(fetched_ms);
        let now_ms = fetched_ms + CACHE_TTL_MS + 1;
        let day = calendar.classify(d(2025, 1, 1), now_ms);
        assert_eq!(day.kind, CalendarKind::Holiday);
        assert!(day.stale);
        assert!(day.diagnostics.iter().any(|diagnostic| matches!(
            diagnostic,
            CalendarDiagnostic::StaleCache {
                publication_year: 2025,
                ..
            }
        )));
        let weekend = calendar.classify(d(2025, 1, 4), now_ms);
        assert_eq!(weekend.kind, CalendarKind::Weekend);
        assert!(weekend.stale);
    }

    #[test]
    fn next_year_publication_wins_for_previous_december() {
        let mut calendar = CalendarData::new();
        calendar
            .insert_cache(cache(
                2024,
                ts_ms(2025, 1, 2, 0, 0),
                &[
                    (d(2024, 12, 31), DayKind::Workday),
                    (d(2024, 12, 30), DayKind::Holiday),
                ],
            ))
            .unwrap();
        calendar
            .insert_cache(cache(
                2025,
                ts_ms(2025, 1, 2, 0, 0),
                &[
                    (d(2024, 12, 31), DayKind::Holiday),
                    (d(2025, 1, 1), DayKind::Holiday),
                ],
            ))
            .unwrap();
        let day = calendar.classify(d(2024, 12, 31), ts_ms(2025, 1, 2, 12, 0));
        assert_eq!(day.kind, CalendarKind::Holiday);
        assert_eq!(day.publication_year, Some(2025));
        // A newer sparse file does not erase the older file's explicit record.
        let older = calendar.classify(d(2024, 12, 30), ts_ms(2025, 1, 2, 12, 0));
        assert_eq!(older.kind, CalendarKind::Holiday);
        assert_eq!(older.publication_year, Some(2024));
    }

    #[test]
    fn december_can_be_covered_by_next_year_cache_alone() {
        let mut calendar = CalendarData::new();
        calendar
            .insert_cache(cache(
                2025,
                ts_ms(2025, 1, 2, 0, 0),
                &[(d(2025, 1, 1), DayKind::Holiday)],
            ))
            .unwrap();
        let friday = calendar.classify(d(2024, 12, 27), ts_ms(2025, 1, 2, 0, 0));
        assert_eq!(friday.kind, CalendarKind::Workday);
        assert_eq!(friday.publication_year, Some(2025));
        let saturday = calendar.classify(d(2024, 12, 28), ts_ms(2025, 1, 2, 0, 0));
        assert_eq!(saturday.kind, CalendarKind::Weekend);
    }
}
