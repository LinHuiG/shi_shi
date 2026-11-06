//! Recurring task templates, deferral times and today's generation. Days
//! and clock times are Shanghai calendar time.

use chrono::{Datelike, Duration, FixedOffset, NaiveDate, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::from_json;
use super::tags::{self, Tag};
use crate::{time, Error, Result};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    pub tags: Vec<Tag>,
    pub id: String,
    pub title: String,
    pub description: String,
    pub frequency: String,
    pub weekdays: Vec<u32>,
    pub time: String,
    pub enabled: bool,
    pub created: i64,
    pub updated: i64,
    pub active_from: i64,
    pub last_generated_on: String,
}

pub fn tomorrow_start(at: i64) -> i64 {
    ((at + 8 * time::HOUR).div_euclid(time::DAY) + 1) * time::DAY - 8 * time::HOUR
}

fn east8() -> FixedOffset {
    FixedOffset::east_opt(8 * 3600).expect("UTC+8")
}

fn shanghai_day(at: i64) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp_millis(at).map(|d| d.with_timezone(&east8()).date_naive())
}

fn local_ms(day: NaiveDate, time: NaiveTime) -> Option<i64> {
    east8().from_local_datetime(&day.and_time(time)).single().map(|d| d.timestamp_millis())
}

/// The moment a deferral choice ends.
pub fn defer_until(choice: &str, work_start: &str, at: i64) -> Result<i64> {
    if choice == "hour" {
        return Ok(at + time::HOUR);
    }
    let day = shanghai_day(at).ok_or("当前时间无效")?;
    let (day, clock) = match choice {
        "afternoon" => (day, NaiveTime::from_hms_opt(13, 0, 0).expect("13:00")),
        "evening" => (day, NaiveTime::from_hms_opt(17, 0, 0).expect("17:00")),
        "tomorrow" | "next_week" => {
            let days = if choice == "tomorrow" { 1 } else { 8 - day.weekday().number_from_monday() };
            let clock =
                NaiveTime::parse_from_str(work_start, "%H:%M").map_err(|_| Error::new("上班时间无效，请检查设置"))?;
            (day + Duration::days(days as i64), clock)
        }
        _ => crate::bail!("延后时间选项无效"),
    };
    let until = local_ms(day, clock).ok_or("延后时间无效")?;
    if until <= at {
        crate::bail!("该延后时刻已过，请重新选择");
    }
    Ok(until)
}

pub fn validate(s: &mut Schedule) -> Result<()> {
    s.title = s.title.trim().into();
    if s.title.is_empty() || s.title.chars().count() > 200 {
        crate::bail!("定时事项标题需要1–200字");
    }
    if s.description.chars().count() > 6000 {
        crate::bail!("事项描述最多6000字");
    }
    if !["daily", "weekly"].contains(&s.frequency.as_str()) {
        crate::bail!("请选择每日或每周");
    }
    let clock = NaiveTime::parse_from_str(&s.time, "%H:%M").map_err(|_| Error::new("请填写有效的生成时间（时:分）"))?;
    s.time = clock.format("%H:%M").to_string();
    s.weekdays.sort_unstable();
    s.weekdays.dedup();
    if s.frequency == "weekly" && (s.weekdays.is_empty() || s.weekdays.iter().any(|d| !(1..=7).contains(d))) {
        crate::bail!("每周事项至少选择一个星期，星期一至星期日分别为1–7");
    }
    if s.frequency == "daily" {
        s.weekdays.clear();
    }
    if s.tags.len() > 40 || s.tags.iter().any(|t| !tags::valid(t)) {
        crate::bail!("定时事项标签无效");
    }
    s.tags.sort();
    s.tags.dedup();
    Ok(())
}

fn occurrence(s: &Schedule, day: NaiveDate) -> Option<i64> {
    if !s.enabled || !["daily", "weekly"].contains(&s.frequency.as_str()) {
        return None;
    }
    if s.frequency == "weekly" && !s.weekdays.contains(&day.weekday().number_from_monday()) {
        return None;
    }
    if s.last_generated_on.as_str() >= day.format("%Y-%m-%d").to_string().as_str() {
        return None;
    }
    let due = local_ms(day, NaiveTime::parse_from_str(&s.time, "%H:%M").ok()?)?;
    (due >= s.active_from).then_some(due)
}

/// Only today's occurrence is caught up, so a long absence leaves no backlog.
pub fn due_today(s: &Schedule, at: i64) -> Option<i64> {
    occurrence(s, shanghai_day(at)?).filter(|due| *due <= at)
}

pub fn next_occurrence(s: &Schedule, at: i64) -> Option<i64> {
    let day = shanghai_day(at)?;
    (0..=7).find_map(|days| occurrence(s, day + Duration::days(days)))
}

pub(crate) fn parse(v: &Value) -> Result<Schedule> {
    let mut object = v.as_object().cloned().unwrap_or_default();
    object.remove("next_at");
    let mut s: Schedule = from_json(&Value::Object(object))?;
    validate(&mut s)?;
    Ok(s)
}

/// `schedule_format` input check, before the AI call.
pub fn format_input(text: &str) -> Result<()> {
    if text.trim().is_empty() || text.chars().count() > 6000 {
        crate::bail!("请输入1–6000字的定时事项描述");
    }
    Ok(())
}

/// Turn the AI's `schedule` answer into an unsaved, enabled draft.
pub fn parse_draft(v: &Value) -> Result<Schedule> {
    if let Some(error) = v["error"].as_str().filter(|e| !e.trim().is_empty()) {
        return Err(Error::new(error));
    }
    let mut s = parse(&v["schedule"])?;
    s.id.clear();
    s.created = 0;
    s.updated = 0;
    s.active_from = 0;
    s.last_generated_on.clear();
    s.enabled = true;
    Ok(s)
}

#[cfg(test)]
mod tests;
