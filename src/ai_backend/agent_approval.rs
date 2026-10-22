//! Approval shape validation shared by retained agent adapters.
use serde_json::{json, Value};
fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap_or("")
}
pub(crate) fn approval_result(request: &Value, p: &Value) -> Result<Value, String> {
    let action = str_at(p, "action");
    let method = str_at(request, "method");
    match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let decision =
                if action == "acceptSimilar" && method == "item/commandExecution/requestApproval" {
                    let rule = request["params"]["proposedExecpolicyAmendment"]
                        .as_array()
                        .filter(|v| {
                            !v.is_empty()
                                && v.iter()
                                    .all(|x| x.as_str().is_some_and(|s| !s.trim().is_empty()))
                        })
                        .ok_or("本次请求未提供类似命令规则")?;
                    json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":rule}})
                } else if ["accept", "acceptForSession", "decline", "cancel"].contains(&action) {
                    json!(action)
                } else {
                    return Err("无效审批选项".into());
                };
            if let Some(options) = request["params"]["availableDecisions"].as_array() {
                if !options.contains(&decision) {
                    return Err("Codex 未允许此审批选项".into());
                }
            }
            Ok(json!({"decision":decision}))
        }
        "item/permissions/requestApproval" => {
            if !["accept", "acceptForSession", "decline"].contains(&action) {
                return Err("请选择允许或拒绝".into());
            }
            Ok(
                json!({"permissions":if action!="decline"{request["params"]["permissions"].clone()}else{json!({})},"scope":if action=="acceptForSession"{"session"}else{"turn"}}),
            )
        }
        "item/tool/requestUserInput" => {
            let questions = request["params"]["questions"]
                .as_array()
                .ok_or("问题格式无效")?;
            let mut answers = serde_json::Map::new();
            for q in questions {
                let id = str_at(q, "id");
                let answer = p["answers"][id].as_str().unwrap_or("").trim();
                if answer.is_empty() {
                    return Err("请回答每个问题")?;
                }
                answers.insert(id.into(), json!({"answers":[answer]}));
            }
            Ok(json!({"answers":answers}))
        }
        "mcpServer/elicitation/request" => {
            if !["decline", "cancel"].contains(&action) {
                return Err("此类外部服务请求请在 Codex 中处理，或拒绝本次请求".into());
            }
            Ok(json!({"action":action,"content":null}))
        }
        _ => Err("该 Codex 请求类型暂不支持，请停止本轮后在 Codex 会话继续处理".into()),
    }
}
