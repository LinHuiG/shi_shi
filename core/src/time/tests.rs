use super::*;

#[test]
fn local_day_and_stamp_use_utc_plus_eight() {
    // 2026-11-03T16:30:00Z is 00:30 on the 4th in UTC+8.
    let at = 1_793_723_400_000;
    assert_eq!(date_from(at), "2026-11-04");
    assert_eq!(stamp(at), "2026-11-04 00:30:00");
    assert_eq!(minute_of_day(at), 30);
    assert_eq!(day_start("2026-11-04"), Some(at - 30 * MINUTE));
    assert_eq!(parse_hm("08:05"), Some(485));
    assert_eq!(parse_hm("8点"), None);
}
