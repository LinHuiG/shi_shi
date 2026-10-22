//! Explicit, opt-in real-provider transport matrix. Reads only saved connection
//! configuration and credentials; never loads a user's messages or tasks.
use crate::{
    ai_backend::{self, AnalysisRequest, ContextTool},
    credentials::CredentialStore,
    model::Config,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct SettingsOnly {
    config: Config,
}
struct CredentialCleanup(PathBuf);
impl Drop for CredentialCleanup {
    fn drop(&mut self) {
        if self.0.is_dir() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
fn arg(args: &[String], name: &str) -> Result<PathBuf, String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .ok_or(format!("missing {name}"))
}
fn minimal(schema: &Value) -> Value {
    if let Some(values) = schema["enum"].as_array() {
        return values[0].clone();
    }
    if let Some(choices) = schema["anyOf"].as_array() {
        return minimal(&choices[0]);
    }
    match schema["type"].as_str().unwrap_or("null") {
        "object" => {
            let mut out = json!({});
            for key in schema["required"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                out[key] = minimal(&schema["properties"][key]);
            }
            out
        }
        "array" => json!((0..schema["minItems"].as_u64().unwrap_or(0))
            .map(|_| minimal(&schema["items"]))
            .collect::<Vec<_>>()),
        "string" => json!(""),
        "boolean" => json!(false),
        "integer" | "number" => json!(schema["minimum"].as_i64().unwrap_or(0)),
        _ => Value::Null,
    }
}
fn outcome(value: &Value) -> Value {
    match value {
        Value::Object(o) => Value::Object(
            o.iter()
                .filter(|(k, _)| k.as_str() != "reason")
                .map(|(k, v)| (k.clone(), outcome(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(outcome).collect()),
        _ => value.clone(),
    }
}
fn model(config: &Config, c: &ai_backend::Connection) -> Result<(String, String), String> {
    let catalog = if c.id.is_empty() {
        &config.codex_models
    } else {
        &c.models
    };
    let chosen = if c.driver == "codex" {
        let preferred = config.for_kind("handling");
        catalog
            .iter()
            .find(|m| m["model"] == preferred.model)
            .or_else(|| catalog.first())
    } else {
        catalog
            .iter()
            .rev()
            .filter(|m| {
                m["model"]
                    .as_str()
                    .is_some_and(|s| s.to_ascii_lowercase().contains("flash"))
            })
            .max_by_key(|m| {
                let id = m["model"].as_str().unwrap_or("").to_ascii_lowercase();
                (if id.contains("4.1") || id.contains("v41") {
                    100
                } else {
                    0
                }) + if id.contains("1017") { 20 } else { 0 }
            })
    }
    .ok_or("No configured Flash model (or Codex model) is available; no model was invented")?;
    let id = chosen["model"]
        .as_str()
        .ok_or("Invalid configured model")?
        .to_string();
    let efforts = chosen["supportedReasoningEfforts"].as_array();
    let effort = ["none", "off", "minimal", "low", "medium"]
        .into_iter()
        .find(|s| efforts.is_some_and(|v| v.iter().any(|x| x["reasoningEffort"] == *s)))
        .unwrap_or("")
        .to_string();
    Ok((id, effort))
}
fn save(path: &Path, report: &Value) -> Result<(), String> {
    fs::write(path, serde_json::to_vec_pretty(report).unwrap()).map_err(|e| e.to_string())
}
fn analysis_route(config: &Config) -> Result<(ai_backend::Connection, String, String), String> {
    let configured = config.for_kind("classify");
    let connection = ai_backend::connection(&configured)?;
    if connection.driver != "api"
        || !connection
            .models
            .iter()
            .any(|m| m["model"] == configured.model)
    {
        return Err(
            "The configured analysis API/model is unavailable; no alternative was selected".into(),
        );
    }
    Ok((connection, configured.model, configured.reasoning_effort))
}
pub fn run(args: &[String]) -> Result<(), String> {
    if !args.iter().any(|a| a == "--allow-live-ai") {
        return Err("Real model tests require --allow-live-ai".into());
    }
    let source = arg(args, "--config-root")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let root = arg(args, "--root")?;
    if root.exists() {
        return Err("Live regression requires a new isolated --root directory".into());
    }
    if !root.is_absolute()
        || root
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Live regression requires an absolute output path without ..".into());
    }
    let parent = root
        .parent()
        .ok_or("Missing output parent")?
        .canonicalize()
        .map_err(|e| e.to_string())?;
    if parent.starts_with(&source) {
        return Err("Test output must be outside the running installation".into());
    }
    fs::create_dir(&root).map_err(|e| e.to_string())?;
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    if root.starts_with(&source) {
        return Err("Test output must be outside the running installation".into());
    }
    let root = PathBuf::from(
        root.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('\\', "/"),
    );
    crate::storage::initialize(&source)?;
    let saved: SettingsOnly = serde_json::from_slice(
        &fs::read(crate::storage::data_dir(&source).join("state.json"))
            .map_err(|e| e.to_string())?,
    )
    .map_err(|_| "Saved connection configuration cannot be read")?;
    let _credential_cleanup =
        CredentialCleanup(crate::storage::data_dir(&root).join("credentials"));
    let mut connections = saved.config.ai_connections.clone();
    connections.insert(
        0,
        ai_backend::Connection {
            name: "Codex".into(),
            executable: saved.config.codex.clone(),
            models: saved.config.codex_models.clone(),
            ..Default::default()
        },
    );
    let mut report = json!({"version":1,"synthetic_only":true,"minimal_prompts":true,"rows":[]});
    let report_path = root.join("report.json");
    let filter = args
        .iter()
        .position(|a| a == "--only-source")
        .and_then(|i| args.get(i + 1));
    let scenario_filter = args
        .iter()
        .position(|a| a == "--only-scenario")
        .and_then(|i| args.get(i + 1));
    let use_analysis_route = args.iter().any(|a| a == "--use-analysis-route");
    if use_analysis_route
        && (filter.map(String::as_str) != Some("api")
            || scenario_filter.map(String::as_str) != Some("classify"))
    {
        return Err(
            "--use-analysis-route requires --only-source api --only-scenario classify".into(),
        );
    }
    for driver in ["codex", "dsh", "api"] {
        if filter.is_some_and(|v| v != driver) {
            continue;
        }
        let selection = if use_analysis_route {
            analysis_route(&saved.config).map(|route| route.0)
        } else {
            connections
                .iter()
                .find(|c| c.driver == driver)
                .cloned()
                .ok_or(format!("Missing configured {driver} source"))
        };
        let mut c = match selection {
            Ok(c) => c,
            Err(e) => {
                report["rows"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"source":driver,"status":"failed","error":e}));
                save(&report_path, &report)?;
                continue;
            }
        };
        if driver == "dsh" {
            let fresh = ai_backend::backend(&c)?.models(&c, &root)?;
            c.models = fresh["models"]
                .as_array()
                .ok_or("DSH did not return a live model catalog")?
                .clone();
        }
        let model_selection = if use_analysis_route {
            analysis_route(&saved.config).map(|(_, model, effort)| (model, effort))
        } else {
            model(&saved.config, &c)
        };
        let (model, effort) = match model_selection {
            Ok(m) => m,
            Err(e) => {
                report["rows"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"source":driver,"status":"failed","error":e}));
                save(&report_path, &report)?;
                continue;
            }
        };
        let mut config = Config::default();
        config.analysis_workdir = root.join("workspace").to_string_lossy().into();
        config.model = model.clone();
        config.reasoning_effort = effort;
        config.retain_ai_traces = true;
        config.codex = c.executable.clone();
        config.codex_models = c.models.clone();
        if driver == "api" {
            let secret =
                CredentialStore::new(crate::storage::data_dir(&source).join("credentials"))
                    .resolve(
                        c.credential_id.as_deref().unwrap_or(&c.id),
                        c.key_env.as_deref(),
                    )
                    .map_err(|e| e.to_string())?
                    .ok_or("Missing API credential")?;
            CredentialStore::new(crate::storage::data_dir(&root).join("credentials"))
                .set("synthetic-live", secret.expose_secret())
                .map_err(|e| e.to_string())?;
            c.credential_id = Some("synthetic-live".into());
            c.key_env = None;
        }
        let backend = ai_backend::backend(&c)?;
        for kind in [
            "message_filter",
            "classify",
            "format",
            "edit",
            "schedule",
            "report",
            "archive",
            "learning",
            "prompt_review",
            "handling_learning",
            "advice_revision",
            "handling",
        ] {
            if scenario_filter.is_some_and(|v| v != kind) {
                continue;
            }
            let start = Instant::now();
            let trace = root.join(format!("{driver}-{kind}"));
            fs::create_dir_all(&trace).map_err(|e| e.to_string())?;
            let outcome = (|| -> Result<Value, String> {
                if kind == "handling" {
                    if driver == "api" {
                        let schema = json!({});
                        let cancel = AtomicBool::new(false);
                        let result = backend.analyze(
                            &c,
                            AnalysisRequest {
                                show_session: false,
                                config: &config,
                                kind,
                                payload: &json!({}),
                                root: &root,
                                trace: &trace,
                                prompt: "synthetic",
                                schema: &schema,
                                tools: None,
                                cancel: &cancel,
                            },
                            &mut |_| {},
                        );
                        return if result.is_err_and(|e| e.contains("不支持处置")) {
                            Ok(json!({"expected_rejection":true,"model_called":false}))
                        } else {
                            Err("API handling was not explicitly rejected".into())
                        };
                    }
                    if driver == "codex" {
                        return desktop_handling(&root, &c, &config, &trace);
                    }
                    let agent = backend.agent(&c)?;
                    let context = ContextTool {
                        cwd: root.join("workspace"),
                        executable: std::env::current_exe().map_err(|e| e.to_string())?,
                        context_path: root.join("unused-synthetic-ticket.json"),
                    };
                    fs::create_dir_all(&context.cwd).map_err(|e| e.to_string())?;
                    let session = agent.start(
                        context.cwd.to_str().unwrap(),
                        "local:synthetic",
                        &context,
                        &config,
                    )?;
                    let thread = session["thread"]["id"].as_str().ok_or("No session id")?;
                    fs::write(trace.join("session.json"), session.to_string())
                        .map_err(|e| e.to_string())?;
                    for text in [
                        "Synthetic transport test. Do not use tools. Reply exactly SS_TEST_OK.",
                        "Reply exactly SS_FOLLOWUP_OK. Do not use tools.",
                    ] {
                        let turn = agent.submit(thread, text, &config, &crate::model::id())?;
                        if turn["turn"]["id"].as_str().is_none() {
                            return Err("No turn id".into());
                        }
                        let deadline = Instant::now() + Duration::from_secs(240);
                        loop {
                            let history = agent.history(thread)?;
                            let rows = history["data"].as_array().ok_or("No history")?;
                            if let Some(row) = rows.first().filter(|r| r["status"] != "inProgress")
                            {
                                fs::write(trace.join("history.json"), history.to_string())
                                    .map_err(|e| e.to_string())?;
                                if row["status"] != "completed" {
                                    return Err(format!(
                                        "Handling did not complete: {}",
                                        row["error"]
                                    ));
                                }
                                let expected = if text.contains("FOLLOWUP") {
                                    "SS_FOLLOWUP_OK"
                                } else {
                                    "SS_TEST_OK"
                                };
                                if !row.to_string().contains(expected) {
                                    return Err(
                                        "Handling response did not match the synthetic marker"
                                            .into(),
                                    );
                                }
                                break;
                            }
                            if Instant::now() >= deadline {
                                return Err("Handling timeout; not resubmitted".into());
                            }
                            std::thread::sleep(Duration::from_millis(250));
                        }
                    }
                    agent.close()?;
                    return Ok(json!({"followup":true,"session_id":thread}));
                }
                let schema = crate::ai::schema(kind);
                let mut expected = minimal(&schema);
                if kind == "message_filter" {
                    expected["decision"] = json!("wait");
                    expected["reason"] = json!("Synthetic notice has no action");
                }
                if kind == "report" {
                    expected["summary"] = json!("Synthetic task complete");
                }
                let protocol = if driver == "dsh" {
                    "Call submit_analysis with exactly this result. Do not use other tools."
                } else {
                    "Return exactly this JSON. Do not use tools."
                };
                let prompt = format!(
                    "Synthetic {kind} test; notice: fixture task complete. {protocol}\n{expected}"
                );
                let payload = json!({});
                let cancel = AtomicBool::new(false);
                let mut public_events = 0_u64;
                let mut public_chars = 0_u64;
                let mut first_public_ms = None;
                let mut output_samples = Vec::new();
                let mut step_metrics = Vec::new();
                let (value, usage) = backend.analyze(
                    &c,
                    AnalysisRequest {
                        show_session: false,
                        config: &config,
                        kind,
                        payload: &payload,
                        root: &root,
                        trace: &trace,
                        prompt: &prompt,
                        schema: &schema,
                        tools: None,
                        cancel: &cancel,
                    },
                    &mut |event| match event {
                        ai_backend::AnalysisEvent::StepFinished { output_tokens, decode_ms } if step_metrics.len()<4096 => {
                            step_metrics.push(json!({"output_tokens":output_tokens,"decode_ms":decode_ms}));
                        }
                        ai_backend::AnalysisEvent::OutputTokens(count) if output_samples.len() < 4096 => {
                            output_samples.push(json!({"elapsed_ms":start.elapsed().as_millis(),"output_tokens":count}));
                        }
                        ai_backend::AnalysisEvent::Stream {kind, text, ..} if matches!(kind.as_str(), "reasoning" | "content") && !text.is_empty() => {
                            public_events += 1;
                            public_chars += text.chars().count() as u64;
                            first_public_ms.get_or_insert(start.elapsed().as_millis());
                        }
                        _ => {}
                    },
                )?;
                let raw: Value = serde_json::from_slice(
                    &fs::read(trace.join("output.txt"))
                        .map_err(|_| "Final output trace missing")?,
                )
                .map_err(|_| "Final output is not JSON")?;
                shishi_acp::schema_validate::validate(&schema, &raw)
                    .map_err(|_| "Final output failed scenario schema")?;
                if outcome(&raw) != outcome(&expected) {
                    return Err("Output differs from the synthetic expected result".into());
                }
                Ok(
                    json!({"usage":usage,"structured_result":value.is_object(),"stream":{"first_public_ms":first_public_ms,"public_events":public_events,"public_chars":public_chars,"reported_output_samples":output_samples,"completed_steps":step_metrics}}),
                )
            })();
            let row = match outcome {
                Ok(detail) => {
                    json!({"source":driver,"scenario":kind,"status":"passed","elapsed_ms":start.elapsed().as_millis(),"detail":detail})
                }
                Err(error) => {
                    json!({"source":driver,"scenario":kind,"status":"failed","elapsed_ms":start.elapsed().as_millis(),"error":error})
                }
            };
            report["rows"].as_array_mut().unwrap().push(row);
            save(&report_path, &report)?;
        }
    }
    let credentials = crate::storage::data_dir(&root).join("credentials");
    if credentials.exists() {
        fs::remove_dir_all(credentials).map_err(|e| e.to_string())?;
    }
    if report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["status"] != "passed")
    {
        return Err(format!(
            "Live matrix has failures; report: {}",
            report_path.display()
        ));
    }
    Ok(())
}

fn desktop_handling(
    root: &Path,
    connection: &ai_backend::Connection,
    config: &Config,
    trace: &Path,
) -> Result<Value, String> {
    use crate::desktop_bridge::{Desktop, DesktopApi};
    let desktop = Desktop::new(root.to_owned(), connection.executable.clone());
    let cwd = root.join("workspace");
    fs::create_dir_all(&cwd).map_err(|e| e.to_string())?;
    let context = ContextTool {
        cwd: cwd.clone(),
        executable: std::env::current_exe().map_err(|e| e.to_string())?,
        context_path: root.join("data/handling-context/synthetic.json"),
    };
    let first = "Synthetic transport test. Do not use tools. Reply exactly SS_TEST_OK.";
    let session = desktop.create(
        cwd.to_str().ok_or("Invalid synthetic directory")?,
        "ShiShi synthetic desktop handling",
        first,
        &config.model,
        &config.reasoning_effort,
        &context,
    )?;
    let thread = session["threadId"]
        .as_str()
        .ok_or("No desktop thread id; not resubmitted")?;
    fs::write(trace.join("session.json"), session.to_string()).map_err(|e| e.to_string())?;
    for (index, expected) in ["SS_TEST_OK", "SS_FOLLOWUP_OK"].iter().enumerate() {
        if index > 0 {
            desktop.send(
                thread,
                "Reply exactly SS_FOLLOWUP_OK. Do not use tools.",
                None,
                None,
            )?;
        }
        let deadline = Instant::now() + Duration::from_secs(240);
        loop {
            let history = desktop.read(thread)?;
            if history["turns"].as_array().is_some_and(|rows| {
                rows.iter().any(|r| {
                    r["status"] == "completed" && r["items"].to_string().contains(expected)
                })
            }) {
                fs::write(trace.join("history.json"), history.to_string())
                    .map_err(|e| e.to_string())?;
                break;
            }
            if Instant::now() >= deadline {
                return Err("Desktop handling timeout; input not resubmitted".into());
            }
            std::thread::sleep(Duration::from_millis(750));
        }
    }
    Ok(json!({"followup":true,"desktop_owned":true,"session_id":thread}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn analysis_probe_uses_exact_saved_route_and_never_falls_back() {
        let mut config = Config::default();
        config.ai_connections = vec![
            ai_backend::Connection {
                id: "first".into(),
                driver: "api".into(),
                models: vec![json!({"model":"first-flash"})],
                ..Default::default()
            },
            ai_backend::Connection {
                id: "selected".into(),
                driver: "api".into(),
                models: vec![json!({"model":"selected-flash"})],
                ..Default::default()
            },
        ];
        config.model_profiles.as_mut().unwrap().analysis = Some(crate::model::ModelChoice {
            connection_id: "selected".into(),
            model: "selected-flash".into(),
            reasoning_effort: "low".into(),
        });
        let (connection, model, effort) = analysis_route(&config).unwrap();
        assert_eq!(
            (connection.id, model, effort),
            ("selected".into(), "selected-flash".into(), "low".into())
        );
        config.ai_connections.pop();
        assert!(analysis_route(&config).is_err());
        config.model_profiles.as_mut().unwrap().analysis = None;
        assert!(analysis_route(&config).is_err());
    }
    #[test]
    fn synthetic_expected_results_cover_every_structured_purpose() {
        for kind in crate::model::PURPOSES {
            if kind == "handling" {
                continue;
            }
            let schema = crate::ai::schema(kind);
            shishi_acp::schema_validate::validate(&schema, &minimal(&schema)).unwrap();
        }
        assert_eq!(
            outcome(&json!({"reason":"synthetic explanation","changes":[]})),
            json!({"changes":[]})
        );
        assert_ne!(
            outcome(&json!({"improved":true})),
            outcome(&json!({"improved":false}))
        );
    }
    #[test]
    fn flash_selection_prefers_configured_1017_and_the_lowest_supported_effort() {
        let config = Config::default();
        let connection = ai_backend::Connection {
            id: "fixture".into(),
            driver: "api".into(),
            models: vec![
                json!({"model":"deepseek-v4-flash","supportedReasoningEfforts":[{"reasoningEffort":"low"}]}),
                json!({"model":"DeepSeek-V4.1-Flash-1017","supportedReasoningEfforts":[{"reasoningEffort":"none"},{"reasoningEffort":"high"}]}),
                json!({"model":"another-model"}),
            ],
            ..Default::default()
        };
        assert_eq!(
            model(&config, &connection).unwrap(),
            ("DeepSeek-V4.1-Flash-1017".into(), "none".into())
        );
    }
}
