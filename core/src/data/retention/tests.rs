use super::*;

#[test]
fn expired_content_is_reduced_to_a_count_only_digest() {
    let mut d = Value::Null;
    assert!(!record(&mut d, "messages", "sources", Vec::<(i64, &str)>::new()));
    assert!(record(&mut d, "messages", "sources", [(5, "a"), (3, "b"), (9, "a")]));
    assert_eq!(d["messages"]["count"], 3);
    assert_eq!(d["messages"]["first"], 3);
    assert_eq!(d["messages"]["last"], 9);
    assert_eq!(d["messages"]["sources"][0], json!({"name": "a", "count": 2}));
    assert!(!d.to_string().contains("text"));
}

#[test]
fn oversized_digest_is_compressed_without_losing_totals() {
    let mut d = Value::Null;
    for i in 0..2000 {
        let name = format!("{i}-{}", "名".repeat(40));
        record(&mut d, "messages", "sources", [(i, name.as_str())]);
        record(&mut d, "records", "kinds", [(i, name.as_str())]);
    }
    compact(&mut d);
    assert!(chars(&d) <= DIGEST_LIMIT);
    assert_eq!(d["messages"]["count"], 2000);
    assert_eq!(d["records"]["count"], 2000);
}
