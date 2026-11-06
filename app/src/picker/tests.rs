use super::*;
use serde_json::json;
#[test]
fn picker_accepts_existing_callers_and_bounded_plugin_filters() {
    for value in [
        json!({}),
        json!({"kind":"插件 ZIP","type":"plugin"}),
        json!({"kind":"程序","type":"file","default_path":"C:\\Synthetic\\app.exe",
            "filters":[{"name":"程序","extensions":["exe"]}]}),
        json!({"kind":"数据目录","type":"directory"}),
    ] {
        assert!(Options::parse(&value).is_ok());
    }
    for value in [
        json!({"type":"shell"}),
        json!({"unknown":"x"}),
        json!({"filters":[{"name":"bad","extensions":["../exe"]}]}),
        json!({"filters":[{"name":"bad","extensions":[]}]}),
        json!({"default_path":"x".repeat(32_769)}),
        json!({"kind":"bad\u{0}title"}),
    ] {
        assert!(Options::parse(&value).is_err());
    }
}
