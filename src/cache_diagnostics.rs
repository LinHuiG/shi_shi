//! Local comparison evidence, not a reconstruction of the provider's rendered request.
//! Only enabled by retain_ai_traces; no extra model calls or conversation reuse.
use crate::ai_backend::{AnalysisRequest, Connection};
use serde_json::{json, Value};
use std::{
    collections::{hash_map::DefaultHasher, VecDeque},
    hash::{Hash, Hasher},
    process::Command,
    sync::{Mutex, OnceLock},
};

// Diagnostic equality fingerprints, not security hashes or provider cache keys.
fn fingerprint(value: impl Hash) -> String {
    let mut hash = DefaultHasher::new();
    value.hash(&mut hash);
    format!("{:016x}", hash.finish())
}

struct Previous {
    group: String,
    prompt: String,
    metadata: Value,
}

fn compare(previous: &Previous, prompt: &str, metadata: &Value) -> Value {
    let common = previous
        .prompt
        .chars()
        .zip(prompt.chars())
        .take_while(|(a, b)| a == b)
        .count();
    let changed = [
        "fixed_prefix",
        "schema",
        "local_tools",
        "cli_arguments",
        "cli_binary",
        "workdir",
        "memory_protocol",
        "image_protocol",
    ]
    .into_iter()
    .filter(|key| previous.metadata[*key] != metadata[*key])
    .collect::<Vec<_>>();
    json!({
        "request_id":previous.metadata["request_id"],
        "gap_ms": metadata["at"].as_i64().unwrap_or(0) - previous.metadata["at"].as_i64().unwrap_or(0),
        "common_prompt_prefix_chars": common,
        "prompt_identical": previous.prompt == prompt,
        "changed_components": changed,
        "baseline":"previous_attempt_same_kind_connection_model_effort_in_this_process"
    })
}

pub fn prepare(r: &AnalysisRequest<'_>, connection: &Connection, command: &Command) -> Value {
    static PREVIOUS: OnceLock<Mutex<VecDeque<Previous>>> = OnceLock::new();
    // Include root to prevent comparison across isolated application instances.
    let group = fingerprint((
        r.root,
        r.kind,
        connection.cache_key(),
        &r.config.model,
        &r.config.reasoning_effort,
    ));
    let fixed = r
        .prompt
        .rsplit_once("\n当前上海时间：")
        .map(|(prefix, _)| prefix);
    let retrieval = r.payload["_local_context"].is_object();
    let tools = r.tools.cloned().unwrap_or_else(|| json!([]));
    let binary = std::fs::metadata(command.get_program()).ok().map(|m| {
        (
            m.len(),
            m.modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis()),
        )
    });
    let mut metadata = json!({
        "version":1,
        "scope":"application_input_only_not_provider_rendered_context",
        "fingerprint_algorithm":"rust_default_hasher_same_build_equality_only",
        "at":crate::model::now(),
        "request_id":r.trace.file_name().and_then(|v|v.to_str()),
        "kind":r.kind,"model":r.config.model,"reasoning_effort":r.config.reasoning_effort,
        "prompt_chars":r.prompt.chars().count(),"prompt_fingerprint":fingerprint(r.prompt),
        "fixed_prefix_chars":fixed.map(|s|s.chars().count()),"fixed_prefix":fixed.map(fingerprint),
        "schema":fingerprint(r.schema.to_string()),"local_tools":fingerprint(tools.to_string()),
        "cli_arguments":fingerprint(command.get_args().collect::<Vec<_>>()),
        "cli_binary":fingerprint((command.get_program(), binary)),
        "workdir":fingerprint(crate::ai::analysis_workdir(r.root,r.config).ok()),
        "retrieval_enabled":retrieval,
        "memory_protocol":r.payload["_local_context"]["memory"].is_object(),
        "image_protocol":r.payload["_local_context"]["images"].is_array(),
        "fresh_conversation":true,"previous":null,"usage":usage(&Value::Null)
    });
    if let Ok(mut previous) = PREVIOUS.get_or_init(|| Mutex::new(VecDeque::new())).lock() {
        if let Some(index) = previous.iter().position(|p| p.group == group) {
            if let Some(old) = previous.remove(index) {
                metadata["previous"] = compare(&old, r.prompt, &metadata);
            }
        }
        // Bounded memory; comparisons restart after application restart. No raw text in summary.
        if previous.len() >= 16 {
            previous.pop_front();
        }
        previous.push_back(Previous {
            group,
            prompt: r.prompt.into(),
            metadata: metadata.clone(),
        });
    }
    metadata
}

pub fn usage(raw: &Value) -> Value {
    let input = raw["input_tokens"].as_u64();
    let cached = raw["cached_input_tokens"]
        .as_u64()
        .or_else(|| raw["input_tokens_details"]["cached_tokens"].as_u64());
    let written = raw["cache_write_input_tokens"].as_u64();
    json!({"input_tokens":input,"cached_input_tokens":cached,"cache_write_input_tokens":written,
        "cache_read_status":match cached {None=>"not_reported",Some(0)=>"zero",Some(_)=>"hit"},
        "cached_input_ratio":input.filter(|n|*n>0).zip(cached).map(|(n,c)| c as f64/n as f64)})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_summary_compares_without_copying_private_input_or_paths() {
        let config = crate::model::Config::default();
        let connection = Connection::default();
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let first = root.join("first");
        let second = root.join("second");
        let payload = json!({"_local_context":{"memory":{}}});
        let schema = json!({"type":"object"});
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let prompt = "synthetic-private-sentinel\n当前上海时间：one";
        let request = AnalysisRequest {
            show_session: true,
            registered_tools: None,
            config: &config,
            kind: "classify",
            payload: &payload,
            root: &root,
            trace: &first,
            prompt,
            schema: &schema,
            tools: None,
            cancel: &cancel,
        };
        let mut command = Command::new("synthetic-cli");
        command.arg("synthetic-private-path");
        let a = prepare(&request, &connection, &command);
        assert!(a["previous"].is_null());
        let request = AnalysisRequest {
            show_session: true,
            registered_tools: None,
            trace: &second,
            prompt: "synthetic-private-sentinel\n当前上海时间：two",
            ..request
        };
        let b = prepare(&request, &connection, &command);
        assert_eq!(b["previous"]["request_id"], "first");
        assert_eq!(b["previous"]["changed_components"], json!([]));
        assert_eq!(a["fixed_prefix"], b["fixed_prefix"]);
        assert_ne!(a["prompt_fingerprint"], b["prompt_fingerprint"]);
        assert!(!b.to_string().contains("synthetic-private"));
        assert!(!b.to_string().contains(&root.to_string_lossy().to_string()));
    }
    #[test]
    fn missing_zero_and_hit_remain_distinct() {
        assert_eq!(usage(&json!({}))["cache_read_status"], "not_reported");
        assert!(usage(&json!({}))["cached_input_tokens"].is_null());
        assert_eq!(
            usage(&json!({"cached_input_tokens":0}))["cache_read_status"],
            "zero"
        );
        let v = usage(&json!({"input_tokens":2000,"input_tokens_details":{"cached_tokens":1000}}));
        assert_eq!(v["cache_read_status"], "hit");
        assert_eq!(v["cached_input_ratio"], 0.5);
        assert!(v["cache_write_input_tokens"].is_null());
        assert!(
            usage(&json!({"input_tokens":0,"cached_input_tokens":0}))["cached_input_ratio"]
                .is_null()
        );
    }
    #[test]
    fn compare_unicode_without_exposing_text_and_detect_protocol_change() {
        let previous = Previous {
            group: String::new(),
            prompt: "固定指令\n时间：早上".into(),
            metadata: json!({"at":10,"request_id":"first","image_protocol":false}),
        };
        let comparison = compare(
            &previous,
            "固定指令\n时间：中午",
            &json!({"at":25,"image_protocol":true}),
        );
        assert_eq!(comparison["common_prompt_prefix_chars"], 8);
        assert_eq!(comparison["gap_ms"], 15);
        assert_eq!(comparison["changed_components"], json!(["image_protocol"]));
        assert!(!comparison.to_string().contains("固定指令"));
    }
}
