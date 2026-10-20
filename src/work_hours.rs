use crate::model::Config;
use chrono::{DateTime, Duration, NaiveTime};

pub const DEFERRED: &str = "当前不是工作或加班状态，AI 已暂停；请确认日历或开启加班后重试";

#[derive(Clone, Copy, PartialEq)]
pub enum AiMode {
    Manual,
    Automatic,
    Nightly,
}
impl AiMode {}

pub fn validate(config: &Config) -> Result<(), String> {
    let start = parse(&config.work_start).ok_or("上班时间格式应为 HH:MM")?;
    let end = parse(&config.work_end).ok_or("下班时间格式应为 HH:MM")?;
    if start == end {
        return Err("上下班时间不能相同；支持跨零点的工作时段".into());
    }
    Ok(())
}
fn parse(value: &str) -> Option<NaiveTime> {
    if value.len() != 5 {
        return None;
    }
    NaiveTime::parse_from_str(value, "%H:%M").ok()
}
pub fn is_work_time(config: &Config, at: i64) -> bool {
    let (Some(start), Some(end), Some(at)) = (
        parse(&config.work_start),
        parse(&config.work_end),
        DateTime::from_timestamp_millis(at),
    ) else {
        return false;
    };
    let current = (at + Duration::hours(8)).time();
    if start < end {
        current >= start && current < end
    } else if start > end {
        current >= start || current < end
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(value: &str) -> i64 {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .timestamp_millis()
    }
    #[test]
    fn default_hours_and_exact_boundaries_use_shanghai_time() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.work_start, "08:00");
        assert_eq!(config.work_end, "20:00");
        for (time, expected) in [
            ("2026-10-15T07:59:59+08:00", false),
            ("2026-10-15T08:00:00+08:00", true),
            ("2026-10-15T19:59:59+08:00", true),
            ("2026-10-15T20:00:00+08:00", false),
            ("2026-10-15T01:00:00Z", true),
        ] {
            assert_eq!(is_work_time(&config, at(time)), expected);
        }
    }
    #[test]
    fn overnight_shifts_and_invalid_settings() {
        let mut config = Config {
            work_start: "22:00".into(),
            work_end: "06:00".into(),
            ..Default::default()
        };
        for (time, expected) in [
            ("2026-10-15T22:00:00+08:00", true),
            ("2026-10-16T00:00:00+08:00", true),
            ("2026-10-16T05:59:59+08:00", true),
            ("2026-10-16T06:00:00+08:00", false),
            ("2026-10-16T21:59:59+08:00", false),
        ] {
            assert_eq!(is_work_time(&config, at(time)), expected);
        }
        assert!(validate(&config).is_ok());
        config.work_end = "22:00".into();
        assert!(validate(&config).is_err());
        config.work_end = "25:00".into();
        assert!(validate(&config).is_err());
        assert!(!is_work_time(&config, at("2026-10-15T22:00:00+08:00")));
    }
}
