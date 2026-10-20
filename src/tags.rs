use crate::{engine::App, model::*};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Tag {
    pub kind: String,
    pub value: String,
}
#[derive(Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Group {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_priority: Option<i32>,
    pub name: String,
    pub values: Vec<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    #[serde(default)]
    pub priority_kind: String,
    #[serde(default)]
    pub assignments: Vec<Assignment>,
    pub id: String,
    pub created: i64,
    pub action: String,
    pub reason: String,
    pub groups: Vec<Group>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Proposal {
    pub archived_at: i64,
    pub id: String,
    pub tag: Tag,
    pub reason: String,
    pub evidence: Vec<String>,
    pub excerpts: Vec<String>,
    pub created: i64,
    pub status: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TagState {
    pub unified: bool,
    pub versions: Vec<Version>,
    pub proposals: Vec<Proposal>,
    pub filters: BTreeMap<String, Vec<String>>,
    pub priorities: Vec<String>,
}

pub fn category_aliases(_s: &State) -> BTreeMap<String, String> {
    BTreeMap::new()
}

pub fn groups(s: &State) -> Vec<Group> {
    let mut groups = s
        .tag_state
        .versions
        .last()
        .map(|v| v.groups.clone())
        .unwrap_or_default();
    let priority = priority_kind(s);
    for group in &mut groups {
        if group.display_priority.is_none() {
            group.display_priority = Some(if group.name == priority {
                10
            } else if group.name == "事项类型" {
                20
            } else {
                100
            });
        }
    }
    groups
}
pub fn known(groups: &[Group], tag: &Tag) -> bool {
    groups
        .iter()
        .any(|g| g.name == tag.kind && g.values.contains(&tag.value))
}
pub fn result_catalog(s: &State, result: &Value) -> Vec<Group> {
    let current = groups(s);
    let mut at_call: Vec<Group> =
        serde_json::from_value(result["_tag_catalog"].clone()).unwrap_or_else(|_| current.clone());
    for g in &mut at_call {
        g.values.retain(|v| {
            known(
                &current,
                &Tag {
                    kind: g.name.clone(),
                    value: v.clone(),
                },
            )
        });
    }
    at_call
}
pub(crate) fn valid(tag: &Tag) -> bool {
    !["__proto__", "prototype", "constructor"].contains(&tag.kind.as_str())
        && !tag.kind.trim().is_empty()
        && tag.kind.trim() == tag.kind
        && tag.kind.chars().count() <= 24
        && !tag.value.trim().is_empty()
        && tag.value.trim() == tag.value
        && tag.value.chars().count() <= 40
        && !tag
            .kind
            .chars()
            .chain(tag.value.chars())
            .any(char::is_control)
}
/// Legacy fields are accepted only at the compatibility boundary. Once converted,
/// clearing a tag cannot be undone by an old fixed field during normalization.
pub fn sync_category_tags(t: &mut Task) {
    for (kind, value) in [
        ("事项等级", std::mem::take(&mut t.priority)),
        ("事项类型", std::mem::take(&mut t.category)),
        ("适用环境", std::mem::take(&mut t.environment)),
    ] {
        if value.trim().is_empty()
            || kind == "适用环境" && ["未指定", "不适用"].contains(&value.as_str())
        {
            continue;
        }
        if !t.tags.iter().any(|tag| tag.kind == kind) {
            t.tags.push(Tag {
                kind: kind.into(),
                value,
            });
        }
    }
    t.tags.retain(valid);
    t.tags.sort();
    t.tags.dedup();
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Assignment {
    pub id: String,
    pub schedule: bool,
    pub before: Vec<Tag>,
    pub after: Vec<Tag>,
}
fn assignment_snapshot(s: &State) -> Vec<Assignment> {
    s.tasks
        .iter()
        .map(|t| Assignment {
            id: t.id.clone(),
            before: t.tags.clone(),
            ..Default::default()
        })
        .chain(s.schedules.iter().map(|t| Assignment {
            id: t.id.clone(),
            schedule: true,
            before: t.tags.clone(),
            ..Default::default()
        }))
        .collect()
}
fn assignment_diff(s: &State, before: Vec<Assignment>) -> Vec<Assignment> {
    before
        .into_iter()
        .filter_map(|mut row| {
            let after = if row.schedule {
                s.schedules
                    .iter()
                    .find(|t| t.id == row.id)
                    .map(|t| t.tags.clone())
            } else {
                s.tasks
                    .iter()
                    .find(|t| t.id == row.id)
                    .map(|t| t.tags.clone())
            }?;
            if row.before == after {
                return None;
            }
            row.after = after;
            Some(row)
        })
        .collect()
}
pub fn priority_kind(s: &State) -> String {
    s.tag_state
        .versions
        .last()
        .map(|v| v.priority_kind.clone())
        .unwrap_or_default()
}
pub fn initialize(s: &mut State) {
    if s.tag_state.unified {
        return;
    }
    let mut catalog = groups(s);
    if s.tag_state.versions.is_empty() {
        catalog = vec![Group {
            display_priority: None,
            name: "事项类型".into(),
            values: ["问题排查", "紧急事项", "日常事务"]
                .map(String::from)
                .to_vec(),
        }];
    }
    if !catalog.iter().any(|g| g.name == "事项等级") {
        catalog.push(Group {
            display_priority: None,
            name: "事项等级".into(),
            values: ["紧急", "高", "普通", "低"].map(String::from).to_vec(),
        });
    }
    for group in &mut catalog {
        if group.name == "适用环境" {
            group
                .values
                .retain(|v| !["未指定", "不适用"].contains(&v.as_str()));
        }
    }
    catalog.retain(|g| !g.values.is_empty());
    let before = assignment_snapshot(s);
    let placeholder =
        |tag: &Tag| tag.kind == "适用环境" && ["未指定", "不适用"].contains(&tag.value.as_str());
    for t in &mut s.tasks {
        sync_category_tags(t);
        t.tags.retain(|tag| !placeholder(tag));
    }
    for t in &mut s.schedules {
        let mut task = Task {
            priority: std::mem::take(&mut t.priority),
            category: std::mem::take(&mut t.category),
            environment: std::mem::take(&mut t.environment),
            tags: std::mem::take(&mut t.tags),
            ..Default::default()
        };
        sync_category_tags(&mut task);
        t.tags = task.tags;
        t.tags.retain(|tag| !placeholder(tag));
    }
    // Keep real existing values rather than losing a user's old classification.
    for tag in s
        .tasks
        .iter()
        .flat_map(|t| &t.tags)
        .chain(s.schedules.iter().flat_map(|t| &t.tags))
    {
        if let Some(group) = catalog.iter_mut().find(|g| g.name == tag.kind) {
            if !group.values.contains(&tag.value) {
                group.values.push(tag.value.clone());
            }
        } else {
            catalog.push(Group {
                display_priority: None,
                name: tag.kind.clone(),
                values: vec![tag.value.clone()],
            });
        }
    }
    if !s.tag_state.priorities.is_empty() {
        s.tag_state
            .filters
            .entry("事项等级".into())
            .or_default()
            .append(&mut s.tag_state.priorities);
    }
    prune_filters(s, &catalog);
    let assignments = assignment_diff(s, before);
    s.tag_state.versions.push(Version {
        id: id(),
        created: now(),
        action: if s.tag_state.versions.is_empty() {
            "initial"
        } else {
            "unify"
        }
        .into(),
        reason: "统一标签目录；预设可维护，兼容旧分类数据".into(),
        groups: catalog,
        priority_kind: "事项等级".into(),
        assignments,
    });
    s.tag_state.unified = true;
}
pub fn sanitize(t: &mut Task, old: Option<&Task>, catalog: &[Group]) {
    sync_category_tags(t);
    t.tags
        .retain(|tag| known(catalog, tag) || old.is_some_and(|o| o.tags.contains(tag)));
    t.tags.sort();
    t.tags.dedup();
}
pub fn effective_tags(t: &Task, catalog: &[Group], priority: &str, at: i64) -> Vec<Tag> {
    let mut tags = t
        .tags
        .iter()
        .filter(|tag| known(catalog, tag))
        .cloned()
        .collect::<Vec<_>>();
    if let Some(group) = catalog
        .iter()
        .find(|g| g.name == priority && !g.values.is_empty())
    {
        let base = tags
            .iter()
            .filter(|t| t.kind == priority)
            .filter_map(|t| group.values.iter().position(|v| v == &t.value))
            .min();
        let deadline = if t.follow_deadline && t.status == "待处理" {
            deadline_ms(&t.due).and_then(|due| {
                let left = due - at;
                if left <= 7_200_000 {
                    Some(1.min(group.values.len() - 1))
                } else if left <= 86_400_000 {
                    Some(2.min(group.values.len() - 1))
                } else {
                    None
                }
            })
        } else {
            None
        };
        if let Some(rank) = base.into_iter().chain(deadline).min() {
            tags.retain(|t| t.kind != priority);
            tags.push(Tag {
                kind: priority.into(),
                value: group.values[rank].clone(),
            });
        }
    }
    tags
}
pub fn priority_rank(tags: &[Tag], catalog: &[Group], priority: &str) -> usize {
    catalog
        .iter()
        .find(|g| g.name == priority)
        .and_then(|g| {
            tags.iter()
                .filter(|t| t.kind == priority)
                .filter_map(|t| g.values.iter().position(|v| v == &t.value))
                .min()
        })
        .unwrap_or(10_000)
}
pub fn snapshot(s: &State) -> Value {
    json!({"unified":true,"priority_kind":priority_kind(s),"groups":groups(s),"versions":s.tag_state.versions,"proposals":s.tag_state.proposals,"filters":s.tag_state.filters,"priorities":s.tag_state.priorities,"category_aliases":category_aliases(s)})
}

/// Archive-generated tag maintenance. Changes are validated against the
/// current catalog and recorded as one new tag version, not as a silent diff.
pub fn apply_archive_changes(
    s: &mut State,
    changes: &[Value],
    _evidence: &BTreeSet<String>,
    at: i64,
) -> Result<usize, String> {
    // Transactional even when called directly: a bad later operation cannot
    // leave earlier catalog or assignment edits half applied.
    let mut next = s.clone();
    let count = apply_catalog(&mut next, changes, at)?;
    *s = next;
    Ok(count)
}
fn prune_filters(s: &mut State, catalog: &[Group]) {
    s.tag_state.filters.retain(|kind, values| {
        values.retain(|value| {
            known(
                catalog,
                &Tag {
                    kind: kind.clone(),
                    value: value.clone(),
                },
            )
        });
        !values.is_empty()
    });
    s.tag_state.priorities.clear();
}
fn apply_catalog(s: &mut State, changes: &[Value], at: i64) -> Result<usize, String> {
    if changes.len() > 40 {
        return Err("标签变更最多40项".into());
    }
    let mut catalog = groups(s);
    let original = catalog.clone();
    let mut priority = priority_kind(s);
    let before = assignment_snapshot(s);
    for change in changes {
        let action = change["action"].as_str().unwrap_or("");
        let tag: Tag =
            serde_json::from_value(change["tag"].clone()).map_err(|_| "标签变更格式无效")?;
        let group_action = ["rename_group", "remove_group", "reorder"].contains(&action);
        if !valid(&Tag {
            kind: tag.kind.clone(),
            value: if group_action {
                "group".into()
            } else {
                tag.value.clone()
            },
        }) {
            return Err("标签名称无效".into());
        }
        let replacement = change
            .get("replacement")
            .filter(|v| !v.is_null())
            .map(|v| serde_json::from_value::<Tag>(v.clone()).map_err(|_| "标签替换格式无效"))
            .transpose()?;
        if action == "add" {
            if let Some(g) = catalog.iter_mut().find(|g| g.name == tag.kind) {
                if !g.values.contains(&tag.value) {
                    g.values.push(tag.value.clone());
                }
            } else {
                catalog.push(Group {
                    display_priority: None,
                    name: tag.kind.clone(),
                    values: vec![tag.value.clone()],
                });
            }
        } else if action == "reorder" {
            let order: Vec<String> =
                serde_json::from_value(change["order"].clone()).map_err(|_| "标签顺序无效")?;
            let g = catalog
                .iter_mut()
                .find(|g| g.name == tag.kind)
                .ok_or("标签类型不存在")?;
            if order.len() != g.values.len()
                || order.iter().collect::<BTreeSet<_>>() != g.values.iter().collect::<BTreeSet<_>>()
            {
                return Err("重排必须恰好包含全部现有标签值".into());
            }
            g.values = order;
        } else if [
            "remove",
            "soft_delete",
            "rename",
            "rename_group",
            "remove_group",
        ]
        .contains(&action)
        {
            let Some(index) = catalog.iter().position(|g| g.name == tag.kind) else {
                continue;
            };
            let group_action = ["rename_group", "remove_group"].contains(&action);
            if !group_action && !catalog[index].values.contains(&tag.value) {
                continue;
            }
            if ["rename", "rename_group"].contains(&action) {
                let target = replacement.as_ref().ok_or("重命名需要新标签")?;
                if !valid(target) {
                    return Err("新标签名称无效".into());
                }
                if action == "rename_group" {
                    if catalog
                        .iter()
                        .any(|g| g.name == target.kind && g.name != tag.kind)
                    {
                        return Err("目标标签类型已存在".into());
                    }
                    catalog[index].name = target.kind.clone();
                    if priority == tag.kind {
                        priority = target.kind.clone();
                    }
                } else {
                    if target.kind != tag.kind {
                        return Err("改名不能改变标签类型；请使用类型改名".into());
                    }
                    if catalog[index].values.contains(&target.value) && target.value != tag.value {
                        return Err("目标标签值已存在".into());
                    }
                    for value in &mut catalog[index].values {
                        if *value == tag.value {
                            *value = target.value.clone();
                        }
                    }
                }
            } else if group_action {
                catalog.remove(index);
                if priority == tag.kind {
                    priority.clear();
                }
            } else {
                catalog[index].values.retain(|v| v != &tag.value);
                if catalog[index].values.is_empty() {
                    catalog.remove(index);
                    if priority == tag.kind {
                        priority.clear();
                    }
                }
            }
            if action == "rename_group" {
                if let Some(values) = s.tag_state.filters.remove(&tag.kind) {
                    s.tag_state
                        .filters
                        .insert(replacement.as_ref().unwrap().kind.clone(), values);
                }
            } else if action == "rename" {
                if let Some(values) = s.tag_state.filters.get_mut(&tag.kind) {
                    for value in values {
                        if *value == tag.value {
                            *value = replacement.as_ref().unwrap().value.clone();
                        }
                    }
                }
            }
            let transform = |tags: &mut Vec<Tag>| {
                tags.retain_mut(|current| {
                    if current.kind != tag.kind || !group_action && current.value != tag.value {
                        return true;
                    }
                    if action == "rename_group" {
                        current.kind = replacement.as_ref().unwrap().kind.clone();
                        true
                    } else if action == "rename" {
                        *current = replacement.as_ref().unwrap().clone();
                        true
                    } else {
                        false
                    }
                });
                tags.sort();
                tags.dedup();
            };
            for t in &mut s.tasks {
                transform(&mut t.tags);
            }
            for t in &mut s.schedules {
                transform(&mut t.tags);
            }
        } else {
            return Err("标签变更动作无效".into());
        }
        if catalog.len() > 32 || catalog.iter().any(|g| g.values.len() > 200) {
            return Err("标签目录超出32类／每类200值限制".into());
        }
    }
    let assignments = assignment_diff(s, before);
    if catalog == original && assignments.is_empty() {
        return Ok(0);
    }
    for row in &assignments {
        if row.schedule {
            if let Some(t) = s.schedules.iter_mut().find(|t| t.id == row.id) {
                t.updated = at.max(t.updated + 1);
            }
        } else if let Some(t) = s.tasks.iter_mut().find(|t| t.id == row.id) {
            t.updated = at.max(t.updated + 1);
        }
    }
    prune_filters(s, &catalog);
    s.tag_state.versions.push(Version {
        id: id(),
        created: at,
        action: "archive".into(),
        reason: format!("AI 维护标签（{} 项）", changes.len()),
        groups: catalog,
        priority_kind: priority,
        assignments,
    });
    Ok(changes.len())
}

// The model only proposes additions. Evidence must come from this invocation, and
// pending/accepted/rejected proposals are retained so polling cannot silently re-enable them.
pub fn collect(s: &mut State, result: &Value, payload: &Value, request: &str) {
    let mut evidence = BTreeMap::new();
    for key in ["changes", "context"] {
        for m in payload[key].as_array().into_iter().flatten() {
            if let Some(mid) = m["id"].as_str() {
                if s.messages.get(mid).is_some_and(|m| !m.withdrawn) {
                    evidence.insert(
                        mid.to_string(),
                        m["text"]
                            .as_str()
                            .unwrap_or("")
                            .chars()
                            .take(350)
                            .collect::<String>(),
                    );
                }
            }
        }
    }
    for a in payload["audit"].as_array().into_iter().flatten() {
        if let Some(i) = a["audit_index"].as_u64() {
            evidence.insert(
                format!("audit:{i}"),
                a["event"]["detail"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(350)
                    .collect(),
            );
        }
    }
    for key in ["user_description", "user_instruction"] {
        if let Some(text) = payload[key].as_str() {
            evidence.insert("input".into(), text.chars().take(350).collect());
        }
    }
    let catalog = groups(s);
    for v in result["tag_suggestions"]
        .as_array()
        .into_iter()
        .flatten()
        .take(8)
    {
        let Ok(tag) = serde_json::from_value::<Tag>(json!({
            "kind":v["kind"],
            "value":v["value"]
        })) else {
            continue;
        };
        let reason = v["reason"].as_str().unwrap_or("").trim();
        let ids = v["evidence_ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !valid(&tag)
            || known(&catalog, &tag)
            || reason.is_empty()
            || ids.is_empty()
            || ids.iter().any(|id| !evidence.contains_key(id))
        {
            continue;
        }
        let proof = ids
            .iter()
            .map(|i| {
                if i == "input" {
                    format!("input:{request}")
                } else {
                    i.clone()
                }
            })
            .collect::<Vec<_>>();
        let excerpts = ids.iter().map(|i| evidence[i].clone()).collect::<Vec<_>>();
        if let Some(p) = s
            .tag_state
            .proposals
            .iter_mut()
            .find(|p| p.tag == tag && p.status != "adopted")
        {
            if p.status == "pending" {
                for (i, e) in proof.iter().zip(excerpts.iter()) {
                    if p.evidence.len() < 20 && !p.evidence.contains(i) {
                        p.evidence.push(i.clone());
                        p.excerpts.push(e.clone());
                    }
                }
            }
        } else if s
            .tag_state
            .proposals
            .iter()
            .filter(|p| p.status == "pending")
            .count()
            < 200
        {
            s.tag_state.proposals.push(Proposal {
                archived_at: 0,
                id: id(),
                tag,
                reason: reason.chars().take(800).collect(),
                evidence: proof,
                excerpts,
                created: now(),
                status: "pending".into(),
            });
        }
    }
}
impl App {
    pub fn tag_filters(&self, v: &Value) -> Result<Value, String> {
        let filters: BTreeMap<String, Vec<String>> =
            serde_json::from_value(v["filters"].clone()).map_err(|e| e.to_string())?;
        if filters.len() > 64
            || filters.iter().any(|(k, vs)| {
                vs.len() > 200
                    || vs.iter().any(|v| {
                        !valid(&Tag {
                            kind: k.clone(),
                            value: v.clone(),
                        })
                    })
            })
        {
            return Err("标签筛选超出范围".into());
        }
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        if let Some(priorities) = v.get("priorities") {
            let mut priorities: Vec<String> =
                serde_json::from_value(priorities.clone()).map_err(|_| "事项等级筛选无效")?;
            let kind = priority_kind(&s);
            let catalog = groups(&s);
            if priorities.len() > 200
                || priorities.iter().any(|value| {
                    !known(
                        &catalog,
                        &Tag {
                            kind: kind.clone(),
                            value: value.clone(),
                        },
                    )
                })
            {
                return Err("事项等级筛选无效".into());
            }
            priorities.sort();
            priorities.dedup();
            s.tag_state.priorities = priorities;
        }
        s.tag_state.filters = filters
            .into_iter()
            .map(|(k, vs)| {
                (
                    k,
                    vs.into_iter()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                )
            })
            .filter(|(_, vs): &(_, Vec<String>)| !vs.is_empty())
            .collect();
        self.save(&s)?;
        let reply = json!(s.tag_state.filters);
        *lock = s;
        Ok(reply)
    }
    pub fn tag_decide(&self, v: &Value) -> Result<Value, String> {
        let mut lock = self.state.lock().unwrap();
        let mut s = lock.clone();
        if v["expected"].as_str() != s.tag_state.versions.last().map(|v| v.id.as_str()) {
            return Err("标签版本已变化，请刷新后重试".into());
        }
        let action = v["action"].as_str().unwrap_or("");
        let mut catalog = groups(&s);
        let mut priority = priority_kind(&s);
        let assignment_before = assignment_snapshot(&s);
        let reason;
        if action == "display_priority" {
            let name = v["name"].as_str().ok_or("缺少标签类型")?;
            let value = v["display_priority"]
                .as_i64()
                .filter(|v| (-10000..=10000).contains(v))
                .ok_or("标签优先级需为 -10000 至 10000 的整数，数值越小越靠前")?
                as i32;
            let group = catalog
                .iter_mut()
                .find(|g| g.name == name)
                .ok_or("标签类型不存在")?;
            if group.display_priority == Some(value) {
                return Ok(snapshot(&s));
            }
            group.display_priority = Some(value);
            reason = format!("调整标签类型 {name} 的显示优先级为 {value}");
        } else if action == "rollback" {
            let version = s
                .tag_state
                .versions
                .iter()
                .find(|r| Some(r.id.as_str()) == v["id"].as_str())
                .ok_or("标签版本不存在")?;
            catalog = version.groups.clone();
            priority = version.priority_kind.clone();
            let target = s
                .tag_state
                .versions
                .iter()
                .position(|r| Some(r.id.as_str()) == v["id"].as_str())
                .unwrap();
            for version in s.tag_state.versions[target + 1..].iter().rev() {
                for row in version.assignments.iter().rev() {
                    let tags = if row.schedule {
                        s.schedules
                            .iter_mut()
                            .find(|t| t.id == row.id)
                            .map(|t| &mut t.tags)
                    } else {
                        s.tasks
                            .iter_mut()
                            .find(|t| t.id == row.id)
                            .map(|t| &mut t.tags)
                    };
                    if let Some(tags) = tags {
                        if *tags == row.after {
                            *tags = row.before.clone();
                        }
                    }
                }
            }
            reason = format!("回滚至 {}", version.id);
        } else {
            let p = s
                .tag_state
                .proposals
                .iter_mut()
                .find(|r| Some(r.id.as_str()) == v["id"].as_str())
                .ok_or("标签建议不存在")?;
            if p.status != "pending" {
                return Err("此建议已处理".into());
            }
            if action == "reject" {
                p.status = "rejected".into();
                self.save(&s)?;
                *lock = s;
                return Ok(json!(true));
            }
            if action != "adopt" {
                return Err("不支持的标签操作".into());
            }
            let tag: Tag = if v.get("tag").is_some() {
                serde_json::from_value(v["tag"].clone()).map_err(|e| e.to_string())?
            } else {
                p.tag.clone()
            };
            if !valid(&tag) {
                return Err("标签类型需1–24字，枚举值需1–40字".into());
            }
            if let Some(g) = catalog.iter_mut().find(|g| g.name == tag.kind) {
                if !g.values.contains(&tag.value) {
                    if g.values.len() >= 200 {
                        return Err("单种标签最多200个枚举值".into());
                    }
                    g.values.push(tag.value.clone());
                }
            } else {
                if catalog.len() >= 32 {
                    return Err("最多32种标签类型".into());
                }
                catalog.push(Group {
                    display_priority: None,
                    name: tag.kind.clone(),
                    values: vec![tag.value.clone()],
                });
            }
            p.status = "adopted".into();
            reason = format!("采用标签建议：{} · {}；{}", tag.kind, tag.value, p.reason);
        }
        prune_filters(&mut s, &catalog);
        let assignments = assignment_diff(&s, assignment_before);
        let changed_at = now();
        for row in &assignments {
            if row.schedule {
                if let Some(t) = s.schedules.iter_mut().find(|t| t.id == row.id) {
                    t.updated = changed_at.max(t.updated + 1);
                }
            } else if let Some(t) = s.tasks.iter_mut().find(|t| t.id == row.id) {
                t.updated = changed_at.max(t.updated + 1);
            }
        }
        let version = Version {
            priority_kind: priority,
            assignments,
            id: id(),
            created: now(),
            action: action.into(),
            reason: reason.clone(),
            groups: catalog,
        };
        s.tag_state.versions.push(version.clone());
        Self::audit(
            &mut s,
            "tag_version",
            "",
            &reason,
            Value::Null,
            json!(version),
        );
        // Neither tasks, analyzed fingerprints, nor the saved filters are changed.
        self.save(&s)?;
        *lock = s;
        Ok(json!(version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn display_priority_persists_rolls_back_and_rejects_stale_or_bad_edits() {
        let app = fixture();
        let original = app
            .state
            .lock()
            .unwrap()
            .tag_state
            .versions
            .last()
            .unwrap()
            .id
            .clone();
        let before = groups(&app.state.lock().unwrap());
        app.tag_decide(&json!({"action":"display_priority","name":"事项类型","display_priority":1,"expected":original})).unwrap();
        assert!(app.tag_decide(&json!({"action":"display_priority","name":"事项类型","display_priority":2,"expected":original})).is_err());
        let loaded = App::load(app.root.clone()).unwrap();
        assert_eq!(
            groups(&loaded.state.lock().unwrap())
                .iter()
                .find(|g| g.name == "事项类型")
                .unwrap()
                .display_priority,
            Some(1)
        );
        let current = loaded
            .state
            .lock()
            .unwrap()
            .tag_state
            .versions
            .last()
            .unwrap()
            .id
            .clone();
        assert!(loaded.tag_decide(&json!({"action":"display_priority","name":"事项类型","display_priority":1.5,"expected":current})).is_err());
        loaded
            .tag_decide(&json!({"action":"rollback","id":original,"expected":current}))
            .unwrap();
        assert!(groups(&loaded.state.lock().unwrap()) == before);
    }
    #[test]
    fn unified_catalog_has_no_environment_defaults_and_empty_tags_stay_empty() {
        let app = fixture();
        let mut s = app.state.lock().unwrap();
        assert!(s.tag_state.unified);
        assert!(!groups(&s).iter().any(|g| g.name == "适用环境"));
        let mut task = Task {
            environment: "未指定".into(),
            ..Default::default()
        };
        normalize_task(&mut task);
        sanitize(&mut task, None, &groups(&s));
        assert!(task.tags.is_empty());
        assert!(task.environment.is_empty());
        let changes = json!([{"action":"remove_group","tag":{"kind":"事项类型","value":""}},{"action":"remove_group","tag":{"kind":"事项等级","value":""}}]);
        apply_archive_changes(&mut s, changes.as_array().unwrap(), &BTreeSet::new(), now())
            .unwrap();
        assert!(groups(&s).is_empty());
        app.save(&s).unwrap();
        drop(s);
        let reloaded = App::load(app.root.clone()).unwrap();
        assert!(groups(&reloaded.state.lock().unwrap()).is_empty());
    }
    #[test]
    fn rename_reorder_delete_and_rollback_preserve_dynamic_ranking_and_assignment_history() {
        let app = fixture();
        let original;
        {
            let mut s = app.state.lock().unwrap();
            original = s.tag_state.versions.last().unwrap().id.clone();
            s.tasks.push(Task {
                id: "fixture-task".into(),
                tags: vec![
                    Tag {
                        kind: "事项等级".into(),
                        value: "低".into(),
                    },
                    Tag {
                        kind: "事项类型".into(),
                        value: "日常事务".into(),
                    },
                ],
                status: "待处理".into(),
                due: "2026-10-19T12:00:00+08:00".into(),
                follow_deadline: true,
                ..Default::default()
            });
            let changes = json!([{"action":"rename","tag":{"kind":"事项等级","value":"高"},"replacement":{"kind":"事项等级","value":"优先"}},{"action":"rename_group","tag":{"kind":"事项等级","value":""},"replacement":{"kind":"处理顺序","value":"组"}},{"action":"remove_group","tag":{"kind":"事项类型","value":""}}]);
            apply_archive_changes(&mut s, changes.as_array().unwrap(), &BTreeSet::new(), now())
                .unwrap();
            assert_eq!(priority_kind(&s), "处理顺序");
            assert_eq!(s.tasks[0].tags.len(), 1);
            assert_eq!(s.tasks[0].tags[0].kind, "处理顺序");
            let effective = effective_tags(
                &s.tasks[0],
                &groups(&s),
                &priority_kind(&s),
                deadline_ms("2026-10-19T11:30:00+08:00").unwrap(),
            );
            assert_eq!(effective[0].value, "优先");
            assert_eq!(s.tasks[0].tags[0].value, "低");
            apply_archive_changes(&mut s,json!([{"action":"reorder","tag":{"kind":"处理顺序","value":""},"order":["低","普通","优先","紧急"]}]).as_array().unwrap(),&BTreeSet::new(),now()).unwrap();
            assert_eq!(priority_rank(&s.tasks[0].tags, &groups(&s), "处理顺序"), 0);
            app.save(&s).unwrap();
        }
        let latest = app
            .state
            .lock()
            .unwrap()
            .tag_state
            .versions
            .last()
            .unwrap()
            .id
            .clone();
        app.tag_decide(&json!({"action":"rollback","id":original,"expected":latest}))
            .unwrap();
        let s = app.state.lock().unwrap();
        assert_eq!(priority_kind(&s), "事项等级");
        assert!(s.tasks[0].tags.iter().any(|t| t.kind == "事项类型"));
        assert!(s.tasks[0]
            .tags
            .iter()
            .any(|t| t.kind == "事项等级" && t.value == "低"));
    }
    #[test]
    fn invalid_catalog_batch_is_atomic_and_legacy_conversion_keeps_real_labels_once() {
        let app = fixture();
        let mut s = app.state.lock().unwrap();
        let before = json!(*s);
        assert!(apply_archive_changes(&mut s,json!([{"action":"add","tag":{"kind":"范围","value":"示例"}},{"action":"reorder","tag":{"kind":"事项等级","value":""},"order":["高"]}]).as_array().unwrap(),&BTreeSet::new(),now()).is_err());
        assert_eq!(json!(*s), before);
        s.tag_state.unified = false;
        s.tasks.push(Task {
            id: "legacy".into(),
            category: "日常事务".into(),
            environment: "示例环境".into(),
            priority: "高".into(),
            ..Default::default()
        });
        initialize(&mut s);
        assert!(s.tasks[0].environment.is_empty());
        assert!(s.tasks[0]
            .tags
            .iter()
            .any(|t| t.kind == "适用环境" && t.value == "示例环境"));
        let len = s.tag_state.versions.len();
        initialize(&mut s);
        assert_eq!(s.tag_state.versions.len(), len);
        apply_archive_changes(
            &mut s,
            json!([{"action":"remove","tag":{"kind":"事项等级","value":"高"}}])
                .as_array()
                .unwrap(),
            &BTreeSet::new(),
            now(),
        )
        .unwrap();
        normalize_task(&mut s.tasks[0]);
        assert!(!s.tasks[0].tags.iter().any(|t| t.value == "高"));
    }
    #[test]
    fn user_categories_are_preserved_when_initializing_again() {
        let app = fixture();
        let mut state = app.state.lock().unwrap();
        state.tag_state.versions.last_mut().unwrap().groups[0]
            .values
            .push("用户分类".into());
        let before = groups(&state);
        initialize(&mut state);
        assert_eq!(
            groups(&state)
                .iter()
                .map(|g| (&g.name, &g.values))
                .collect::<Vec<_>>(),
            before
                .iter()
                .map(|g| (&g.name, &g.values))
                .collect::<Vec<_>>()
        );
        assert!(category_aliases(&state).is_empty());
    }
    #[test]
    fn priority_filters_persist_and_reject_invalid_values_atomically() {
        let app = fixture();
        app.tag_filters(&json!({"filters":{"事项类型":["日常事务"]},"priorities":["紧急","高"]}))
            .unwrap();
        assert!(app
            .tag_filters(&json!({"filters":{},"priorities":["不存在"]}))
            .is_err());
        let restored = App::load(app.root.clone()).unwrap();
        let s = restored.state.lock().unwrap();
        assert_eq!(s.tag_state.priorities, vec!["紧急", "高"]);
        assert_eq!(s.tag_state.filters["事项类型"], vec!["日常事务"]);
    }
    fn fixture() -> App {
        App::load(std::env::temp_dir().join(format!("shishi-tags-{}", id()))).unwrap()
    }
    #[test]
    fn proposals_require_real_evidence_and_do_not_apply_until_adopted() {
        let app = fixture();
        let before = app.state.lock().unwrap().tasks.clone();
        {
            let mut s = app.state.lock().unwrap();
            collect(
                &mut s,
                &json!({"tag_suggestions":[{"kind":"所属项目","value":"示例项目","reason":"用户说明","evidence_ids":["fake"]}]}),
                &json!({"user_description":"示例项目的验证"}),
                "r",
            );
            assert!(s.tag_state.proposals.is_empty());
            collect(
                &mut s,
                &json!({"tag_suggestions":[{"kind":"所属项目","value":"示例项目","reason":"用户说明","evidence_ids":["input"]}]}),
                &json!({"user_description":"示例项目的验证"}),
                "r",
            );
            assert_eq!(s.tag_state.proposals.len(), 1);
            assert!(!known(&groups(&s), &s.tag_state.proposals[0].tag));
        }
        let s = app.state.lock().unwrap().clone();
        let first = s.tag_state.versions[0].id.clone();
        let p = &s.tag_state.proposals[0];
        let adopted = app
            .tag_decide(&json!({"id":p.id,"action":"adopt","expected":first}))
            .unwrap();
        assert!(app
            .tag_decide(&json!({"id":p.id,"action":"adopt","expected":first}))
            .is_err());
        assert_eq!(json!(app.state.lock().unwrap().tasks), json!(before));
        app.tag_filters(&json!({"filters":{"所属项目":["示例项目"]}}))
            .unwrap();
        app.tag_decide(&json!({"id":first,"action":"rollback","expected":adopted["id"]}))
            .unwrap();
        let restored = App::load(app.root.clone()).unwrap();
        let s = restored.state.lock().unwrap();
        assert_eq!(s.tag_state.versions.len(), 3);
        assert!(!s.tag_state.filters.contains_key("所属项目"));
        assert!(!known(&groups(&s), &p.tag));
    }
    #[test]
    fn unknown_tags_cannot_silently_expand_catalog_and_rollback_keeps_history() {
        let app = fixture();
        let s = app.state.lock().unwrap();
        let catalog = groups(&s);
        let mut t = Task {
            category: "任意新类".into(),
            environment: "未指定".into(),
            tags: vec![
                Tag {
                    kind: "任意类型".into(),
                    value: "未知值".into(),
                },
                Tag {
                    kind: "提出者".into(),
                    value: "协作者".into(),
                },
            ],
            ..Default::default()
        };
        sanitize(&mut t, None, &catalog);
        assert!(t.category.is_empty());
        assert!(t.tags.is_empty());
        let old = Task {
            tags: vec![Tag {
                kind: "历史类型".into(),
                value: "历史值".into(),
            }],
            ..t.clone()
        };
        t.tags = old.tags.clone();
        sanitize(&mut t, Some(&old), &catalog);
        assert!(t.tags.contains(&old.tags[0]));
    }
    #[test]
    fn failed_save_does_not_change_filters() {
        let app = fixture();
        std::fs::create_dir(app.root.join("data/state.next.json")).unwrap();
        assert!(app
            .tag_filters(&json!({"filters":{"提出者":["产品"]}}))
            .is_err());
        assert!(app.state.lock().unwrap().tag_state.filters.is_empty());
    }
}
