use crate::model::{Schedule, Task};
use chrono::{Datelike, Duration, FixedOffset, NaiveDate, NaiveTime, TimeZone};

pub fn tomorrow_start(at: i64) -> i64 {
    ((at + 8 * 3_600_000).div_euclid(86_400_000) + 1) * 86_400_000 - 8 * 3_600_000
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
