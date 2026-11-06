//! The structured output contract of every purpose and the read-only
//! retrieval tool declarations. Field order is part of the model contract.

use serde_json::{json, Value};

use crate::prompts::memory::{ARCHIVED_LIMIT, TEMPORARY_LIMIT};

const NO_ACTION_REASON_LIMIT: usize = 240;

/// Field declarations and their required order change together.
macro_rules! extend_schema {
    ($schema:expr, {$($name:expr => $field:expr),* $(,)?}) => {{
        let schema = &mut *($schema);
        for (name, field) in [$(($name, $field)),*] {
            schema["properties"][name] = field;
            schema["required"].as_array_mut().unwrap().push(json!(name));
        }
    }};
}

fn tag_schema(changes: bool) -> Value {
    let mut schema = json!({"type":"object","additionalProperties":false,"properties":{"kind":{"type":"string"},"value":{"type":"string"}},"required":["kind","value"]});
    if changes {
        schema["properties"]["action"] = json!({"type":"string","enum":["add","remove"]});
        schema["required"].as_array_mut().unwrap().insert(0, json!("action"));
    }
    schema
}

/// The structured output contract of a purpose.
pub fn schema(kind: &str) -> Value {
    if kind == "message_filter" {
        return json!({"type":"object","additionalProperties":false,"properties":{"decision":{"type":"string","enum":["analyze","wait","uncertain"]},"reason":{"type":"string","maxLength":200}},"required":["decision","reason"]});
    }
    let mut schema = base_schema(kind);
    if ["classify", "format", "edit", "learning"].contains(&kind) {
        let mut proposal = tag_schema(false);
        extend_schema!(&mut proposal, {
            "reason" => json!({"type":"string"}),
            "evidence_ids" => json!({"type":"array","items":{"type":"string"}}),
        });
        extend_schema!(&mut schema, {
            "tag_suggestions" => json!({"type":"array","maxItems":8,"items":proposal}),
        });
    }
    if kind == "classify" || kind == "format" {
        let key = if kind == "classify" { "actions" } else { "tasks" };
        let fields = &mut schema["properties"][key]["items"];
        let tag_field = if kind == "classify" { "tag_changes" } else { "tags" };
        extend_schema!(fields, {
            "tool_call_ids" => json!({"type":"array","maxItems":64,"items":{"type":"string","maxLength":200}}),
            "suggested_project_id" => json!({"type":"string","maxLength":400}),
            "suggested_project_reason" => json!({"type":"string","maxLength":400}),
            tag_field => json!({"type":"array","maxItems":40,"items":tag_schema(kind == "classify")}),
        });
        if kind == "classify" {
            extend_schema!(fields, {
                "fields" => json!({"type":"array","items":{"type":"object","additionalProperties":false,"properties":{"field":{"type":"string","enum":["title","description","summary","status","blocker","waiting_for","due","deferred_until"]},"value":{"type":"string","maxLength":600}},"required":["field","value"]}}),
                "deferral_reason" => json!({"type":"string","maxLength":400,"default":""}),
                "deferral_evidence" => json!({"type":"array","items":{"type":"string"},"default":[]}),
                "change_summary" => json!({"type":"string","maxLength":30,"default":""}),
            });
            extend_schema!(&mut schema, {
                "no_action_reason" => json!({"type":"string","maxLength":NO_ACTION_REASON_LIMIT,
                    "description":"When actions is empty, provide a non-blank conclusion grounded in the supplied messages. Do not invent actions to avoid explaining a no-action result. May be empty only when actions is non-empty."}),
                "image_notes" => json!({"type":"array","maxItems":2,"items":{"type":"object","additionalProperties":false,"properties":{"image_id":{"type":"string"},"summary":{"type":"string","maxLength":300}},"required":["image_id","summary"]}}),
                "memory_update" => json!({"type":"array","maxItems":1,"items":{"type":"string","maxLength":TEMPORARY_LIMIT}}),
            });
        }
    } else if kind == "schedule" {
        extend_schema!(&mut schema["properties"]["schedule"], {
            "tags" => json!({"type":"array","maxItems":40,"items":tag_schema(false)}),
        });
    } else if kind == "edit" {
        extend_schema!(&mut schema, {
            "tag_changes" => json!({"type":"array","maxItems":40,"items":tag_schema(true)}),
            "suggested_project_id" => json!({"type":"string","maxLength":400}),
            "suggested_project_reason" => json!({"type":"string","maxLength":400}),
        });
    }
    schema
}

fn all_required_object(properties: Value) -> Value {
    let mut schema = json!({"type":"object","additionalProperties":false});
    schema["required"] = properties.as_object().unwrap().keys().map(|name| json!(name)).collect();
    schema["properties"] = properties;
    schema
}

fn base_schema(kind: &str) -> Value {
    let text = || json!({"type":"string"});
    match kind {
        "archive" => {
            let archived_memory =
                json!({"type":"array","maxItems":1,"items":{"type":"string","maxLength":ARCHIVED_LIMIT}});
            json!({"type":"object","additionalProperties":false,"properties":{"report":schema("report"),"archived_memory":archived_memory},"required":["report","archived_memory"]})
        }
        "advice_revision" => {
            json!({"type":"object","additionalProperties":false,"properties":{"title":text(),"rule":text(),"reason":text(),"prompt":text(),"tag":{"type":"object","additionalProperties":false,"properties":{"kind":text(),"value":text()},"required":["kind","value"]}},"required":["title","rule","reason","prompt","tag"]})
        }
        "prompt_review" => {
            json!({"type":"object","additionalProperties":false,"properties":{"improved":{"type":"boolean"},"reason":text(),"proposed_prompt":text(),"evidence_ids":{"type":"array","items":text()}},"required":["improved","reason","proposed_prompt","evidence_ids"]})
        }
        "edit" => {
            json!({"type":"object","additionalProperties":false,"properties":{"reason":text(),"changes":{"type":"array","items":{"type":"object","additionalProperties":false,"properties":{"field":{"type":"string","enum":["title","description","status","blocker","waiting_for","due"]},"value":text()},"required":["field","value"]}}},"required":["reason","changes"]})
        }
        "schedule" => {
            let item = all_required_object(
                json!({"title":text(),"description":text(),"frequency":{"type":"string","enum":["daily","weekly",""]},"weekdays":{"type":"array","items":{"type":"integer","minimum":1,"maximum":7}},"time":text()}),
            );
            json!({"type":"object","additionalProperties":false,"properties":{"error":text(),"schedule":item},"required":["error","schedule"]})
        }
        "report" => {
            json!({"type":"object","additionalProperties":false,"properties":{"summary":{"type":"string","maxLength":600},"completed":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":240}},"attention":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":240}},"next_steps":{"type":"array","maxItems":5,"items":{"type":"string","maxLength":240}}},"required":["summary","completed","attention","next_steps"]})
        }
        "format" => {
            let item = all_required_object(json!({"title":text(),"description":text(),"blocker":text(),"due":text()}));
            json!({"type":"object","additionalProperties":false,"properties":{"tasks":{"type":"array","minItems":1,"items":item}},"required":["tasks"]})
        }
        "learning" => {
            json!({"type":"object","additionalProperties":false,"properties":{"suggestions":{"type":"array","maxItems":3,"items":{"type":"object","additionalProperties":false,"properties":{"suggestion_id":text(),"title":text(),"rule":text(),"evidence":{"type":"array","items":{"type":"integer"}}},"required":["suggestion_id","title","rule","evidence"]}}},"required":["suggestions"]})
        }
        _ => {
            let props = if kind == "classify" {
                json!({"action":{"type":"string","enum":["create","update"]},"task_id":text(),
                    "reason":{"type":"string","maxLength":160},"confidence":{"type":"number"},
                    "sources":{"type":"array","items":text()},"background_evidence":{"type":"array","items":text()},
                    "completion_evidence":{"type":"array","items":text()},"deadline_evidence":{"type":"array","items":text()}})
            } else {
                json!({"action":text(),"task_id":text(),"title":text(),"description":text(),"status":text(),"blocker":text(),"waiting_for":text(),"due":text(),"reason":text(),"confidence":{"type":"number"},"sources":{"type":"array","items":text()},"background_evidence":{"type":"array","items":text()},"completion_evidence":{"type":"array","items":text()},"deadline_evidence":{"type":"array","items":text()}})
            };
            let item = all_required_object(props);
            json!({"type":"object","additionalProperties":false,"properties":{"actions":{"type":"array","items":item}},"required":["actions"]})
        }
    }
}

/// Read-only retrieval tools offered when the payload has `_local_context`.
pub fn context_tools() -> Value {
    let mut value = json!({"tools":[{"name":"read_context","description":"只读查询获授权的分析上下文（消息/事项/更新流水），可跨来源和会话按id、关键词、来源、会话、类型和时间范围检索；事项按updated时间过滤，来源/会话仅在其全部依据一致时可用，更新流水不支持来源/会话过滤；长文本用next_offset分页续读，每次返回有上限，可重复按需查询；重复页和已读区域不会重复返回，进程受取消和超时控制。","inputSchema":{"type":"object","additionalProperties":false,"properties":{"ids":{"type":"array","items":{"type":"string"}},"query":{"type":"string","maxLength":80},"offset":{"type":"integer","minimum":0},"source":{"type":"string","maxLength":128},"chat":{"type":"string","maxLength":128},"kind":{"type":"string","enum":["message","task","update"]},"since":{"type":"integer"},"until":{"type":"integer"}},"required":[]},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}]});
    value["tools"][0]["inputSchema"]["properties"]["chat"]["description"] =
        json!("本次输入中的 rooms.id（自动绑定来源），或真实会话编号；不按展示名称匹配");
    value["tools"].as_array_mut().unwrap().push(json!({"name":"read_image","description":"按image_id查看本批相关消息的本地图片，最多2次；只在文字不足时使用。未下载则不可用。","inputSchema":{"type":"object","additionalProperties":false,"properties":{"image_id":{"type":"string","maxLength":512}},"required":["image_id"]},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    value
}
