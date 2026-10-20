//! State-file encoding only. Runtime/IPC archive generations remain complete,
//! independent documents; equal prompt bodies are written once per state file.
use crate::model::ArchiveGroup;
use serde::{ser::SerializeSeq, Deserialize, Deserializer, Serializer};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub fn serialize<S: Serializer>(groups: &[ArchiveGroup], serializer: S) -> Result<S::Ok, S::Error> {
    let mut seen = BTreeMap::<&str, &str>::new();
    let mut sequence = serializer.serialize_seq(Some(groups.len()))?;
    for group in groups {
        let mut row = serde_json::to_value(group).map_err(serde::ser::Error::custom)?;
        if !group.prompt.is_empty() && !group.id.is_empty() {
            if let Some(previous) = seen
                .get(group.prompt.as_str())
                .filter(|id| **id != group.id)
            {
                row["prompt"] = json!("");
                row["prompt_ref"] = json!({"version":1,"group":previous});
            } else {
                seen.insert(&group.prompt, &group.id);
            }
        }
        sequence.serialize_element(&row)?;
    }
    sequence.end()
}

pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<ArchiveGroup>, D::Error> {
    let rows = Vec::<Value>::deserialize(deserializer)?;
    let mut prompts = BTreeMap::<String, String>::new();
    let mut groups = Vec::with_capacity(rows.len());
    for mut row in rows {
        if let Some(reference) = row.as_object_mut().and_then(|o| o.remove("prompt_ref")) {
            if reference["version"] != 1 || row["prompt"] != "" {
                return Err(serde::de::Error::custom("invalid archive prompt reference"));
            }
            let id = reference["group"]
                .as_str()
                .ok_or_else(|| serde::de::Error::custom("invalid archive prompt source"))?;
            let prompt = prompts
                .get(id)
                .ok_or_else(|| serde::de::Error::custom("missing archive prompt source"))?;
            row["prompt"] = json!(prompt);
        }
        let group: ArchiveGroup = serde_json::from_value(row).map_err(serde::de::Error::custom)?;
        prompts.insert(group.id.clone(), group.prompt.clone());
        groups.push(group);
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::State;
    #[test]
    fn inline_and_reference_forms_round_trip_and_pruning_rebases_storage() {
        let mut state = State::default();
        for id in ["a", "b", "c"] {
            state.archive_versions.push(ArchiveGroup {
                id: id.into(),
                prompt: "synthetic policy".repeat(100),
                ..Default::default()
            });
        }
        let stored = serde_json::to_value(&state).unwrap();
        assert!(stored["archive_versions"][1]["prompt_ref"].is_object());
        let mut loaded: State = serde_json::from_value(stored).unwrap();
        assert_eq!(
            loaded.archive_versions[0].prompt,
            loaded.archive_versions[2].prompt
        );
        loaded.archive_versions.remove(0);
        let stored = serde_json::to_value(&loaded).unwrap();
        assert!(!stored["archive_versions"][0]["prompt"]
            .as_str()
            .unwrap()
            .is_empty());
        assert_eq!(stored["archive_versions"][1]["prompt_ref"]["group"], "b");
        let mut broken = stored;
        broken["archive_versions"][1]["prompt_ref"]["group"] = json!("missing");
        assert!(serde_json::from_value::<State>(broken).is_err());
    }
}
