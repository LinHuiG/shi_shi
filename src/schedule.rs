use crate::model::{Schedule, Task};
use chrono::{Datelike, Duration, FixedOffset, NaiveDate, NaiveTime, TimeZone};

pub fn tomorrow_start(at: i64) -> i64 {
    ((at + 8 * 3_600_000).div_euclid(86_400_000) + 1) * 86_400_000 - 8 * 3_600_000
}

/// Explicit deferrals use Shanghai calendar time, independent of workday gates.
pub fn defer_until(choice: &str, work_start: &str, at: i64) -> Result<i64, String> {
    if choice == "hour" {
        return Ok(at + 3_600_000);
    }
    let day = shanghai_day(at).ok_or("当前时间无效")?;
    let (day, time) = match choice {
        "afternoon" => (day, NaiveTime::from_hms_opt(13, 0, 0).unwrap()),
        "evening" => (day, NaiveTime::from_hms_opt(17, 0, 0).unwrap()),
        "tomorrow" | "next_week" => {
            let days = if choice == "tomorrow" {
                1
            } else {
                8 - day.weekday().number_from_monday()
            };
            (
                day + Duration::days(days as i64),
                NaiveTime::parse_from_str(work_start, "%H:%M")
                    .map_err(|_| "上班时间无效，请检查设置")?,
            )
        }
        _ => return Err("延后时间选项无效".into()),
    };
    let until = FixedOffset::east_opt(8 * 3600)
        .unwrap()
        .from_local_datetime(&day.and_time(time))
        .single()
        .ok_or("延后时间无效")?
        .timestamp_millis();
    if until <= at {
        return Err("该延后时刻已过，请重新选择".into());
    }
    Ok(until)
}

pub fn validate(s: &mut Schedule) -> Result<(), String> {
    s.title = s.title.trim().into();
    if s.title.is_empty() || s.title.chars().count() > 200 {
        return Err("定时事项标题需要1–200字".into());
    }
    if s.description.chars().count() > 6000 {
        return Err("事项描述最多6000字".into());
    }
    if !["daily", "weekly"].contains(&s.frequency.as_str()) {
        return Err("请选择每日或每周".into());
    }
    let time =
        NaiveTime::parse_from_str(&s.time, "%H:%M").map_err(|_| "请填写有效的生成时间（时:分）")?;
    s.time = time.format("%H:%M").to_string();
    s.weekdays.sort_unstable();
    s.weekdays.dedup();
    if s.frequency == "weekly"
        && (s.weekdays.is_empty() || s.weekdays.iter().any(|d| !(1..=7).contains(d)))
    {
        return Err("每周事项至少选择一个星期，星期一至星期日分别为1–7".into());
    }
    if s.frequency == "daily" {
        s.weekdays.clear();
    }
    let mut task = Task {
        priority: std::mem::take(&mut s.priority),
        category: std::mem::take(&mut s.category),
        tags: std::mem::take(&mut s.tags),
        environment: std::mem::take(&mut s.environment),
        ..Default::default()
    };
    crate::model::normalize_task(&mut task);
    if task.tags.len() > 40 || task.tags.iter().any(|tag| !crate::tags::valid(tag)) {
        return Err("定时事项标签无效".into());
    }
    s.tags = task.tags;
    Ok(())
}

fn shanghai_day(at: i64) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp_millis(at).map(|d| {
        d.with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap())
            .date_naive()
    })
}

fn occurrence(s: &Schedule, day: NaiveDate) -> Option<i64> {
    if !s.enabled || (s.frequency != "daily" && s.frequency != "weekly") {
        return None;
    }
    if s.frequency == "weekly" && !s.weekdays.contains(&day.weekday().number_from_monday()) {
        return None;
    }
    if s.last_generated_on.as_str() >= day.format("%Y-%m-%d").to_string().as_str() {
        return None;
    }
    let time = NaiveTime::parse_from_str(&s.time, "%H:%M").ok()?;
    let due = FixedOffset::east_opt(8 * 3600)?
        .from_local_datetime(&day.and_time(time))
        .single()?
        .timestamp_millis();
    (due >= s.active_from).then_some(due)
}

// Catch up only today's occurrence, avoiding a backlog after a long absence.
pub fn due_today(s: &Schedule, at: i64) -> Option<i64> {
    occurrence(s, shanghai_day(at)?).filter(|due| *due <= at)
}

pub fn next_occurrence(s: &Schedule, at: i64) -> Option<i64> {
    let day = shanghai_day(at)?;
    (0..=7).find_map(|days| occurrence(s, day + Duration::days(days)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::deadline_ms;
    fn at(s: &str) -> i64 {
        deadline_ms(s).unwrap()
    }
    #[test]
    fn deferral_calendar_and_expired_choices() {
        let now = at("2026-10-28T12:30:00+08:00");
        for (choice, expected) in [
            ("hour", "2026-10-28T13:30:00+08:00"),
            ("afternoon", "2026-10-28T13:00:00+08:00"),
            ("evening", "2026-10-28T17:00:00+08:00"),
            ("tomorrow", "2026-10-29T08:45:00+08:00"),
            ("next_week", "2026-11-02T08:45:00+08:00"),
        ] {
            assert_eq!(defer_until(choice, "08:45", now).unwrap(), at(expected));
        }
        assert!(defer_until("afternoon", "08:45", at("2026-10-28T13:00:00+08:00")).is_err());
        assert!(defer_until("evening", "08:45", at("2026-10-28T17:00:00+08:00")).is_err());
        assert_eq!(
            defer_until("next_week", "09:00", at("2026-11-02T08:00:00+08:00")).unwrap(),
            at("2026-11-09T09:00:00+08:00")
        );
        assert_eq!(
            defer_until("tomorrow", "09:00", at("2026-12-31T23:59:00+08:00")).unwrap(),
            at("2027-01-01T09:00:00+08:00")
        );
        assert!(defer_until("tomorrow", "invalid", now).is_err());
    }
    fn daily() -> Schedule {
        Schedule {
            title: "巡检".into(),
            frequency: "daily".into(),
            time: "09:00".into(),
            enabled: true,
            active_from: at("2026-10-14T10:00:00+08:00"),
            ..Default::default()
        }
    }
    #[test]
    fn due_time_and_shanghai_midnight() {
        let mut s = daily();
        assert!(due_today(&s, at("2026-10-15T08:59:59+08:00")).is_none());
        assert!(due_today(&s, at("2026-10-15T09:00:00+08:00")).is_some());
        s.last_generated_on = "2026-10-15".into();
        assert!(due_today(&s, at("2026-10-15T23:59:59+08:00")).is_none());
        assert_eq!(
            next_occurrence(&s, at("2026-10-15T23:59:59+08:00")),
            Some(at("2026-10-16T09:00:00+08:00"))
        );
        assert_eq!(
            tomorrow_start(at("2026-12-31T23:59:59+08:00")),
            at("2027-01-01T00:00:00+08:00")
        );
    }
    #[test]
    fn weekly_multiple_days_and_new_or_paused_schedule() {
        let mut s = daily();
        s.frequency = "weekly".into();
        s.weekdays = vec![1, 5];
        assert!(due_today(&s, at("2026-10-15T10:00:00+08:00")).is_none());
        assert!(due_today(&s, at("2026-10-16T09:00:00+08:00")).is_some());
        s.active_from = at("2026-10-16T10:00:00+08:00");
        assert_eq!(
            next_occurrence(&s, s.active_from),
            Some(at("2026-10-19T09:00:00+08:00"))
        );
        s.enabled = false;
        assert!(next_occurrence(&s, s.active_from).is_none());
    }
    #[test]
    fn validates_schedule() {
        let mut s = daily();
        validate(&mut s).unwrap();
        s.time = "25:00".into();
        assert!(validate(&mut s).is_err());
        s.time = "09:00".into();
        s.frequency = "weekly".into();
        assert!(validate(&mut s).is_err());
        s.weekdays = vec![0, 8];
        assert!(validate(&mut s).is_err());
        s.weekdays = vec![5, 1, 5];
        validate(&mut s).unwrap();
        assert_eq!(s.weekdays, vec![1, 5]);
    }
}
