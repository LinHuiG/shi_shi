use serde_json::json;

use super::*;
use crate::tasks::model::deadline_ms;

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
    assert_eq!(defer_until("tomorrow", "invalid", now).unwrap_err().0, "上班时间无效，请检查设置");
    assert_eq!(defer_until("someday", "09:00", now).unwrap_err().0, "延后时间选项无效");
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
    assert_eq!(next_occurrence(&s, at("2026-10-15T23:59:59+08:00")), Some(at("2026-10-16T09:00:00+08:00")));
    assert_eq!(tomorrow_start(at("2026-12-31T23:59:59+08:00")), at("2027-01-01T00:00:00+08:00"));
}

#[test]
fn weekly_multiple_days_and_new_or_paused_schedule() {
    let mut s = daily();
    s.frequency = "weekly".into();
    s.weekdays = vec![1, 5];
    assert!(due_today(&s, at("2026-10-15T10:00:00+08:00")).is_none());
    assert!(due_today(&s, at("2026-10-16T09:00:00+08:00")).is_some());
    s.active_from = at("2026-10-16T10:00:00+08:00");
    assert_eq!(next_occurrence(&s, s.active_from), Some(at("2026-10-19T09:00:00+08:00")));
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

#[test]
fn draft_is_enabled_and_unsaved_or_reports_the_ai_error() {
    let draft = parse_draft(&json!({"schedule": {
        "id": "x", "title": " 周报 ", "frequency": "weekly", "weekdays": [5], "time": "9:30",
        "created": 5, "updated": 5, "last_generated_on": "2026-01-01", "next_at": 1
    }}))
    .unwrap();
    assert_eq!((draft.id.as_str(), draft.title.as_str(), draft.time.as_str()), ("", "周报", "09:30"));
    assert!(draft.enabled && draft.updated == 0 && draft.last_generated_on.is_empty());
    assert_eq!(parse_draft(&json!({"error": "无法识别"})).unwrap_err().0, "无法识别");
    assert_eq!(format_input("  ").unwrap_err().0, "请输入1–6000字的定时事项描述");
}
