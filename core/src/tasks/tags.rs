//! The tag catalog: versioned groups, proposals, saved filters, and how a
//! task's tags become its effective tags and priority rank.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::model::{deadline_ms, from_json, Task};
use crate::{time, Error, Result};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Tag {
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Group {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_priority: Option<i32>,
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Assignment {
    pub id: String,
    pub schedule: bool,
    pub before: Vec<Tag>,
    pub after: Vec<Tag>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Version {
    pub priority_kind: String,
    pub assignments: Vec<Assignment>,
    pub id: String,
    pub created: i64,
    pub action: String,
    pub reason: String,
    pub groups: Vec<Group>,
}

pub fn valid(tag: &Tag) -> bool {
    !["__proto__", "prototype", "constructor"].contains(&tag.kind.as_str())
        && !tag.kind.trim().is_empty()
        && tag.kind.trim() == tag.kind
        && tag.kind.chars().count() <= 24
        && !tag.value.trim().is_empty()
        && tag.value.trim() == tag.value
        && tag.value.chars().count() <= 40
        && !tag.kind.chars().chain(tag.value.chars()).any(char::is_control)
}

pub fn known(groups: &[Group], kind: &str, value: &str) -> bool {
    groups.iter().any(|g| g.name == kind && g.values.iter().any(|v| v == value))
}

pub(crate) fn fill_priorities(groups: &mut [Group], kind: &str) {
    for g in groups {
        if g.display_priority.is_none() {
            g.display_priority = Some(if g.name == kind {
                10
            } else if g.name == "事项类型" {
                20
            } else {
                100
            });
        }
    }
}

/// The catalog an AI call saw, narrowed to values still in the current one.
pub fn result_catalog(current: &[Group], result: &Value) -> Vec<Group> {
    let Ok(mut at_call) = from_json::<Vec<Group>>(&result["_tag_catalog"]) else {
        return current.to_vec();
    };
    let available: BTreeSet<(&str, &str)> =
        current.iter().flat_map(|g| g.values.iter().map(move |v| (g.name.as_str(), v.as_str()))).collect();
    for g in &mut at_call {
        g.values.retain(|v| available.contains(&(g.name.as_str(), v.as_str())));
    }
    at_call
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    action: String,
    kind: String,
    value: String,
}

/// Apply an AI tag patch atomically; empty changes keep every tag.
pub fn apply_changes(current: &[Tag], changes: &Value, catalog: &[Group], allow_remove: bool) -> Result<Vec<Tag>> {
    let changes: Vec<Change> =
        from_json(changes).map_err(|_| Error::new("标签变更格式无效，需要 action、kind、value；事项未保存"))?;
    if changes.len() > 40 {
        crate::bail!("标签变更超过 40 项，事项未保存");
    }
    let mut seen = BTreeSet::new();
    let mut result: BTreeSet<Tag> = current.iter().cloned().collect();
    for change in changes {
        let tag = Tag { kind: change.kind, value: change.value };
        if !valid(&tag) || !seen.insert(tag.clone()) {
            crate::bail!("标签变更包含非法、重复或冲突的标签，事项未保存");
        }
        match change.action.as_str() {
            "add" if known(catalog, &tag.kind, &tag.value) => {
                result.insert(tag);
            }
            "add" => crate::bail!("新增标签不在本次可用目录中，请先采用标签建议或重新分析；事项未保存"),
            "remove" if allow_remove => {
                result.remove(&tag);
            }
            "remove" => crate::bail!("新建事项只能添加标签，不能删除已有标签；事项未保存"),
            _ => crate::bail!("标签变更只支持 add/remove；事项未保存"),
        }
    }
    if result.len() > 40 {
        crate::bail!("事项标签超过 40 个，事项未保存");
    }
    Ok(result.into_iter().collect())
}

/// Drop tags outside the catalog unless the task already had them.
pub fn sanitize(t: &mut Task, old: Option<&Task>, catalog: &[Group]) {
    t.tags.retain(valid);
    t.tags.retain(|tag| known(catalog, &tag.kind, &tag.value) || old.is_some_and(|o| o.tags.contains(tag)));
    t.tags.sort();
    t.tags.dedup();
}

/// Catalog tags, with the priority raised as the deadline nears: within 2 h
/// to the second level, within 24 h to the third.
pub fn effective_tags(t: &Task, catalog: &[Group], priority: &str, at: i64) -> Vec<Tag> {
    let mut tags: Vec<Tag> = t.tags.iter().filter(|tag| known(catalog, &tag.kind, &tag.value)).cloned().collect();
    if let Some(group) = catalog.iter().find(|g| g.name == priority && !g.values.is_empty()) {
        let base = tags
            .iter()
            .filter(|t| t.kind == priority)
            .filter_map(|t| group.values.iter().position(|v| v == &t.value))
            .min();
        let last = group.values.len() - 1;
        let deadline =
            (t.follow_deadline && t.active()).then(|| deadline_ms(&t.due)).flatten().and_then(|due| match due - at {
                left if left <= 2 * time::HOUR => Some(1.min(last)),
                left if left <= time::DAY => Some(2.min(last)),
                _ => None,
            });
        if let Some(rank) = base.into_iter().chain(deadline).min() {
            tags.retain(|t| t.kind != priority);
            tags.push(Tag { kind: priority.into(), value: group.values[rank].clone() });
        }
    }
    tags
}

pub fn priority_rank(tags: &[Tag], catalog: &[Group], priority: &str) -> usize {
    catalog
        .iter()
        .find(|g| g.name == priority)
        .and_then(|g| {
            tags.iter().filter(|t| t.kind == priority).filter_map(|t| g.values.iter().position(|v| v == &t.value)).min()
        })
        .unwrap_or(10_000)
}
