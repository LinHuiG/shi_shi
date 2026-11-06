//! Which manual changes the learning pass may cite as evidence.

use serde_json::Value;

/// Whether a manual change is evidence the learning pass may cite.
pub fn learning_evidence(kind: &str, detail: &str, after: &Value) -> bool {
    matches!(kind, "manual_edit" | "manual_ai_edit" | "undo" | "merge")
        || kind == "manual_status" && ["dismiss", "reopen"].contains(&detail)
        || kind == "manual_archive" && after["archive_note"].as_str().is_some_and(|n| !n.trim().is_empty())
}
