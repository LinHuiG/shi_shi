use super::*;

#[test]
fn values_round_trip_and_counters_increment() {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::data::schema::migrate(&mut conn).unwrap();
    assert_eq!(get::<String>(&conn, "a").unwrap(), None);
    set(&conn, "a", "x").unwrap();
    assert_eq!(get::<String>(&conn, "a").unwrap().as_deref(), Some("x"));
    assert_eq!(bump(&conn, "n").unwrap(), 1);
    assert_eq!(bump(&conn, "n").unwrap(), 2);
    assert_eq!(get_i64(&conn, "n").unwrap(), 2);
}
