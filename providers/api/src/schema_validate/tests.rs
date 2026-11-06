use super::*;
use serde_json::json;

#[test]
fn the_subset_validates_and_defaults_only_repair_output() {
    let schema = json!({"type":"object","additionalProperties":false,"properties":{
        "n":{"type":"integer","minimum":1,"maximum":3},
        "tags":{"type":"array","maxItems":2,"items":{"type":"string","enum":["a","b"]}},
        "actions":{"type":"array","items":{"type":"object","additionalProperties":false,
            "properties":{"status":{"type":"string","enum":["pending","done"]},
                "summary":{"type":"string","maxLength":3,"default":""}},"required":["status","summary"]}}
    },"required":["n"]});
    assert!(validate(&schema, &json!({"n":2,"tags":["a"]})).is_ok());
    for (bad, path) in [
        (json!({}), "$.n"),
        (json!({"n":4}), "$.n"),
        (json!({"n":1.5}), "$.n"),
        (json!({"n":1,"x":1}), "$"),
        (json!({"n":1,"tags":["c"]}), "$.tags[0]"),
        (json!({"n":1,"tags":["a","a","a"]}), "$.tags"),
    ] {
        assert_eq!(validate(&schema, &bad).unwrap_err().path, path, "{bad}");
    }
    for summary in [json!(null), json!("long"), json!(["x"])] {
        let value = json!({"n":1,"actions":[{"status":"pending","summary":summary}]});
        assert!(validate(&schema, &value).is_err(), "tool arguments stay strict");
        assert_eq!(validate_output(&schema, value).unwrap()["actions"][0]["summary"], "");
    }
    let missing = validate_output(&schema, json!({"n":1,"actions":[{"status":"done"}]})).unwrap();
    assert_eq!(missing["actions"][0]["summary"], "");
    assert!(validate_output(&schema, json!({"n":1,"actions":[{"status":"bad"}]})).is_err());
    assert_eq!(ValidationError::new("$.n", "x").to_string(), "schema at $.n: x");
}
