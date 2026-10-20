//! Local update watermarks. Original send/insert times never serve as cursors.
//! The watermark records the latest acknowledged revision; sparse pending
//! exceptions preserve unprocessed holes and legacy fingerprints during upgrade.
use crate::model::*;

// A watermark may pass an older unprocessed revision (for example outside
// the current lookback). Keep only those holes, so widening the scope cannot
// silently turn them into successes. Existing maps also support legacy data.
const PENDING: &str = "pending:";
pub fn pending_revision(m: &Message, stored: Option<&String>) -> bool {
    stored
        .and_then(|v| v.strip_prefix(PENDING))
        .is_some_and(|v| v == fingerprint(m))
}
pub fn analyzed(s: &State, m: &Message) -> bool {
    if pending_revision(m, s.analyzed.get(&m.id)) {
        return false;
    }
    (m.updated_at > 0
        && m.updated_at <= s.analysis_updated_at
        && !s.analysis_retries.contains_key(&m.id))
        || fingerprint_matches(m, s.analyzed.get(&m.id))
}
pub fn filtered(s: &State, m: &Message) -> bool {
    if analyzed(s, m) {
        return true;
    }
    if pending_revision(m, s.message_filter.checked.get(&m.id)) {
        return false;
    }
    (m.updated_at > 0 && m.updated_at <= s.filter_updated_at)
        || fingerprint_matches(m, s.message_filter.checked.get(&m.id))
}
pub fn insert(s: &mut State, mut m: Message) {
    let old = s.messages.get(&m.id);
    m.inserted_at = old.map(|m| m.inserted_at).unwrap_or_else(now);
    m.updated_at = match old {
        Some(old) if fingerprint(old) == fingerprint(&m) => old.updated_at,
        _ => {
            // Strictly increasing even if many messages arrive in the same ms
            // or the wall clock moves backward. No timestamp/ID tie can be lost.
            s.message_updated_at = now().max(s.message_updated_at.saturating_add(1));
            s.message_updated_at
        }
    };
    s.messages.insert(m.id.clone(), m);
}
pub fn initialize(s: &mut State) {
    s.message_updated_at = s
        .message_updated_at
        .max(s.messages.values().map(|m| m.updated_at).max().unwrap_or(0));
    let mut legacy = s
        .messages
        .values()
        .filter(|m| m.updated_at == 0)
        .map(|m| (m.time, m.id.clone()))
        .collect::<Vec<_>>();
    legacy.sort();
    for (_, id) in legacy {
        s.message_updated_at = now().max(s.message_updated_at.saturating_add(1));
        let m = s.messages.get_mut(&id).unwrap();
        m.updated_at = s.message_updated_at;
        // Historical local insertion time is unknown; keep the original send
        // time as a fallback, never replace it with the upgrade time.
        if m.inserted_at == 0 {
            m.inserted_at = m.time;
        }
    }
    advance_analysis(s);
    advance_filter(s);
}
pub fn advance_analysis(s: &mut State) {
    let pending = s
        .messages
        .values()
        .filter(|m| !m.withdrawn && !analyzed(s, m))
        .map(|m| {
            (
                m.id.clone(),
                m.updated_at,
                format!("{PENDING}{}", fingerprint(m)),
            )
        })
        .collect::<Vec<_>>();
    let acknowledged = s
        .analyzed
        .iter()
        .filter_map(|(id, fp)| {
            s.messages
                .get(id)
                .filter(|m| fingerprint_matches(m, Some(fp)))
                .map(|m| m.updated_at)
        })
        .max()
        .unwrap_or(0);
    let withdrawn = s
        .messages
        .values()
        .filter(|m| m.withdrawn)
        .map(|m| m.updated_at)
        .max()
        .unwrap_or(0);
    s.analysis_updated_at = s.analysis_updated_at.max(acknowledged).max(withdrawn);
    s.analyzed.retain(|id, _| {
        s.messages
            .get(id)
            .is_some_and(|m| m.updated_at == 0 || m.updated_at > s.analysis_updated_at)
    });
    for (id, updated, record) in pending {
        if s.analysis_updated_at > 0 && updated <= s.analysis_updated_at {
            s.analyzed.insert(id, record);
        }
    }
}
pub fn advance_filter(s: &mut State) {
    let pending = s
        .messages
        .values()
        .filter(|m| !m.withdrawn && !filtered(s, m))
        .map(|m| {
            (
                m.id.clone(),
                m.updated_at,
                format!("{PENDING}{}", fingerprint(m)),
            )
        })
        .collect::<Vec<_>>();
    let acknowledged = s
        .message_filter
        .checked
        .iter()
        .filter_map(|(id, fp)| {
            s.messages
                .get(id)
                .filter(|m| fingerprint_matches(m, Some(fp)))
                .map(|m| m.updated_at)
        })
        .max()
        .unwrap_or(0);
    s.filter_updated_at = s
        .filter_updated_at
        .max(s.analysis_updated_at)
        .max(acknowledged);
    s.message_filter.checked.retain(|id, _| {
        s.messages
            .get(id)
            .is_some_and(|m| m.updated_at == 0 || m.updated_at > s.filter_updated_at)
    });
    for (id, updated, record) in pending {
        if s.filter_updated_at > 0 && updated <= s.filter_updated_at {
            s.message_filter.checked.insert(id, record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn out_of_scope_old_messages_do_not_stall_watermarks_or_disappear_after_widening() {
        let mut s = State::default();
        let at = now();
        insert(
            &mut s,
            Message {
                id: "old".into(),
                text: "Old pending".into(),
                time: at - 72 * 3_600_000,
                ..Default::default()
            },
        );
        insert(
            &mut s,
            Message {
                id: "done".into(),
                text: "Handled".into(),
                time: at,
                ..Default::default()
            },
        );
        s.analyzed
            .insert("done".into(), fingerprint(&s.messages["done"]));
        advance_analysis(&mut s);
        advance_filter(&mut s);
        assert_eq!(s.analysis_updated_at, s.messages["done"].updated_at);
        assert_eq!(s.filter_updated_at, s.analysis_updated_at);
        assert!(crate::context_budget::pending(&s).is_empty());
        assert!(!analyzed(&s, &s.messages["old"]));
        assert!(!filtered(&s, &s.messages["old"]));
        assert!(filtered(&s, &s.messages["done"]));
        let mut restored: State =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        initialize(&mut restored);
        restored.config.analysis_lookback_hours = 96;
        let pending = crate::context_budget::pending(&restored);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "old");
        assert!(!filtered(&restored, &pending[0]));
    }

    #[test]
    fn newly_scanned_messages_are_not_covered_by_an_older_completed_batch() {
        let mut s = State::default();
        for id in ["a", "b"] {
            insert(
                &mut s,
                Message {
                    id: id.into(),
                    text: "Body".into(),
                    time: now(),
                    ..Default::default()
                },
            );
        }
        let frozen = s.messages.clone();
        let boundary = s.message_updated_at;
        insert(
            &mut s,
            Message {
                id: "new".into(),
                text: "Arrived while analyzing".into(),
                time: now(),
                ..Default::default()
            },
        );
        for (id, m) in &frozen {
            s.message_filter.checked.insert(id.clone(), fingerprint(m));
            s.analyzed.insert(id.clone(), fingerprint(m));
        }
        advance_analysis(&mut s);
        advance_filter(&mut s);
        assert_eq!(s.analysis_updated_at, boundary);
        assert_eq!(s.filter_updated_at, boundary);
        assert!(!analyzed(&s, &s.messages["new"]));
        assert!(!filtered(&s, &s.messages["new"]));
        assert_eq!(crate::context_budget::pending(&s).len(), 1);
    }

    #[test]
    fn updates_preserve_original_times_and_failed_batches_keep_the_analysis_cursor() {
        let mut s = State::default();
        for id in ["a", "b", "c"] {
            insert(
                &mut s,
                Message {
                    id: id.into(),
                    text: "body".into(),
                    time: 1,
                    ..Default::default()
                },
            );
        }
        let original = s.messages["a"].clone();
        let boundary = s.messages["b"].updated_at;
        let mut renamed = original.clone();
        renamed.source_name = "new name".into();
        renamed.reply = "reply".into();
        insert(&mut s, renamed);
        assert_eq!(s.messages["a"].updated_at, original.updated_at);
        for id in ["a", "b"] {
            s.message_filter
                .checked
                .insert(id.into(), fingerprint(&s.messages[id]));
        }
        advance_filter(&mut s);
        assert_eq!(s.filter_updated_at, boundary);
        assert_eq!(s.analysis_updated_at, 0);
        assert!(s.message_filter.checked.is_empty());
        // Batch one succeeded, batch two failed: retry starts at c.
        for id in ["a", "b"] {
            s.analyzed.insert(id.into(), fingerprint(&s.messages[id]));
        }
        advance_analysis(&mut s);
        assert_eq!(s.analysis_updated_at, boundary);
        assert!(s.analyzed.is_empty());
        assert!(analyzed(&s, &s.messages["a"]));
        assert!(!analyzed(&s, &s.messages["c"]));
        let mut edited = original.clone();
        edited.text = "edited".into();
        insert(&mut s, edited);
        assert!(s.messages["a"].updated_at > s.messages["c"].updated_at);
        assert_eq!(s.messages["a"].time, original.time);
        assert_eq!(s.messages["a"].inserted_at, original.inserted_at);
        assert!(!analyzed(&s, &s.messages["a"]));
        assert!(!filtered(&s, &s.messages["a"]));
    }
    #[test]
    fn a_hole_does_not_block_the_cursor_and_remains_pending() {
        let mut s = State::default();
        for id in ["a", "b", "c"] {
            insert(
                &mut s,
                Message {
                    id: id.into(),
                    text: "body".into(),
                    ..Default::default()
                },
            );
        }
        s.analyzed.insert("c".into(), fingerprint(&s.messages["c"]));
        advance_analysis(&mut s);
        assert_eq!(s.analysis_updated_at, s.messages["c"].updated_at);
        assert!(!analyzed(&s, &s.messages["a"]));
        assert!(analyzed(&s, &s.messages["c"]));
        assert!(pending_revision(&s.messages["a"], s.analyzed.get("a")));
        advance_filter(&mut s);
        assert_eq!(s.filter_updated_at, s.analysis_updated_at);
        assert!(!filtered(&s, &s.messages["a"]));
        assert!(filtered(&s, &s.messages["c"]));
        for id in ["a", "b"] {
            s.analyzed.insert(id.into(), fingerprint(&s.messages[id]));
        }
        advance_analysis(&mut s);
        assert_eq!(s.analysis_updated_at, s.message_updated_at);
        assert!(s.analyzed.is_empty());
    }
}
