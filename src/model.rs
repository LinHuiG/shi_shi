use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The only state format this build can load. Product data written before the
/// analysis redesign is intentionally not migrated: startup lists incompatible
/// managed files and offers a scoped cleanup instead of silently defaulting.
pub const STATE_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub auto_update: bool,
    pub ai_connections: Vec<crate::ai_backend::Connection>,
    #[serde(skip)]
    pub selected_connection: String,
    #[serde(default = "legacy_model_profiles")]
    pub model_profiles: Option<ModelProfiles>,
    pub message_retention_days: u32,
    pub handling_prompt: String,
    pub tool_call_prompt: String,
    pub tools: Vec<crate::tool_registry::ToolDefinition>,
    pub review_prompt: String,
    pub archive_prompt: String,
    pub codex: String,
    pub codex_models: Vec<Value>,
    pub codex_name: String,
    pub ai_timeout_seconds: u64,
    pub analysis_workdir: String,
    pub interval_minutes: u64,
    pub analysis_interval_minutes: u64,
    pub analysis_lookback_hours: u64,
    /// Optional relevance gate; off includes all unanalysed incremental messages.
    pub analysis_related_only: bool,
    pub leaders: Vec<String>,
    pub enabled: bool,
    pub auto_ai: bool,
    pub message_filter_enabled: bool,
    pub auto_adopt_advice: bool,
    /// Retired archive automation flags. Read old settings without activating them.
    pub auto_adopt_prompt: bool,
    pub retain_ai_traces: bool,
    pub work_start: String,
    pub work_end: String,
    pub daily_calls: u64,
    pub max_input_chars: usize,
    /// Per-request cap on characters returned by read-only retrieval, counted
    /// separately from the first-turn `max_input_chars`.
    #[serde(alias = "context_read_chars")]
    pub retrieval_budget_chars: usize,
    /// Retired preference: accept old files, but choose the backdrop by runtime capability.
    #[serde(skip_serializing)]
    pub window_backdrop: bool,
    pub model: String,
    pub reasoning_effort: String,
    pub classify_prompt: String,
    pub format_prompt: String,
    pub learning_prompt: String,
    pub report_prompt: String,
}
/// Every purpose that can reach a provider. Adding a purpose here forces
/// connection validation, work-state gating and usage accounting to cover it.
pub const PURPOSES: [&str; 12] = [
    "classify",
    "format",
    "edit",
    "schedule",
    "report",
    "learning",
    "prompt_review",
    "handling",
    "handling_learning",
    "advice_revision",
    "archive",
    "message_filter",
];
fn default_model(effort: &str) -> ModelChoice {
    ModelChoice {
        connection_id: String::new(),
        model: "gpt-6-astra".into(),
        reasoning_effort: effort.into(),
    }
}
fn legacy_model_profiles() -> Option<ModelProfiles> {
    Some(ModelProfiles {
        filter: Some(default_model("low")),
        analysis: Some(default_model("low")),
        handling: Some(default_model("medium")),
        review: Some(default_model("high")),
        legacy_tool_call: None,
    })
}
fn legacy_model_choice() -> Option<ModelChoice> {
    Some(ModelChoice::default())
}
pub const UNCONFIGURED_CONNECTION: &str = "__unconfigured_purpose__";
impl Default for Config {
    fn default() -> Self {
        Self {
            auto_update: true,
            ai_connections: vec![],
            selected_connection: String::new(),
            model_profiles: Some(ModelProfiles::default()),
            message_retention_days: 90,
            handling_prompt: String::new(),
            tool_call_prompt: String::new(),
            tools: vec![],
            review_prompt: String::new(),
            archive_prompt: String::new(),
            codex: String::new(),
            codex_models: vec![],
            codex_name: "默认 Codex".into(),
            ai_timeout_seconds: 600,
            analysis_workdir: "ShiShi".into(),
            interval_minutes: 3,
            analysis_interval_minutes: 30,
            analysis_lookback_hours: 24,
            analysis_related_only: false,
            leaders: vec![],
            enabled: true,
            auto_ai: true,
            message_filter_enabled: true,
            auto_adopt_advice: false,
            auto_adopt_prompt: false,
            retain_ai_traces: false,
            work_start: "08:00".into(),
            work_end: "20:00".into(),
            daily_calls: 150,
            max_input_chars: 80_000,
            retrieval_budget_chars: 80_000,
            window_backdrop: true,
            model: "gpt-6-astra".into(),
            reasoning_effort: "low".into(),
            classify_prompt: String::new(),
            format_prompt: String::new(),
            learning_prompt: String::new(),
            report_prompt: String::new(),
        }
    }
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelChoice {
    pub connection_id: String,
    pub model: String,
    pub reasoning_effort: String,
}
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelProfiles {
    /// Retired independent route, preserved only when reading an older configuration.
    #[serde(rename = "tool_call", skip_serializing_if = "Option::is_none")]
    pub legacy_tool_call: Option<ModelChoice>,
    #[serde(default = "legacy_model_choice")]
    pub filter: Option<ModelChoice>,
    #[serde(default = "legacy_model_choice")]
    pub analysis: Option<ModelChoice>,
    #[serde(default = "legacy_model_choice")]
    pub handling: Option<ModelChoice>,
    #[serde(default = "legacy_model_choice")]
    pub review: Option<ModelChoice>,
}
impl Config {
    pub fn automatic_analysis_enabled(&self) -> bool {
        self.enabled && self.auto_ai
    }
    pub fn early_analysis_enabled(&self) -> bool {
        self.automatic_analysis_enabled() && self.message_filter_enabled
    }
    pub fn for_kind(&self, kind: &str) -> Self {
        let mut c = self.clone();
        if let Some(p) = &self.model_profiles {
            let choice = match kind {
                "message_filter" => &p.filter,
                "handling" => &p.handling,
                "archive" | "learning" | "prompt_review" | "report" | "handling_learning"
                | "advice_revision" => &p.review,
                _ => &p.analysis,
            };
            let Some(choice) = choice else {
                c.selected_connection = UNCONFIGURED_CONNECTION.into();
                c.model.clear();
                c.reasoning_effort.clear();
                return c;
            };
            // A legacy partial profile (for example one saved before the filter
            // purpose existed) keeps the base value for fields it does not set.
            // It never silently becomes an empty model or connection.
            if !choice.connection_id.is_empty() {
                c.selected_connection = choice.connection_id.clone();
            }
            if !choice.model.trim().is_empty() {
                c.model = choice.model.clone();
            }
            if !choice.reasoning_effort.trim().is_empty() {
                c.reasoning_effort = choice.reasoning_effort.clone();
            }
        }
        c
    }
    pub fn clear_connection_routes(&mut self, id: &str) {
        let resolved =
            ["message_filter", "classify", "handling", "archive"].map(|kind| self.for_kind(kind));
        let materialized = |c: &Config| {
            Some(ModelChoice {
                connection_id: c.selected_connection.clone(),
                model: c.model.clone(),
                reasoning_effort: c.reasoning_effort.clone(),
            })
        };
        let p = self.model_profiles.get_or_insert_with(|| ModelProfiles {
            filter: materialized(&resolved[0]),
            analysis: materialized(&resolved[1]),
            handling: materialized(&resolved[2]),
            review: materialized(&resolved[3]),
            legacy_tool_call: None,
        });
        for (choice, c) in [
            &mut p.filter,
            &mut p.analysis,
            &mut p.handling,
            &mut p.review,
        ]
        .into_iter()
        .zip(resolved)
        {
            if c.selected_connection == id {
                *choice = None;
            }
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Message {
    /// Local ingest timestamps. `time` remains the source's original send time.
    /// updated_at advances only for a new body/withdrawal revision.
    pub inserted_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageRef>,
    pub source_id: String,
    pub source_name: String,
    pub external_id: String,
    pub id: String,
    pub chat: String,
    pub from: String,
    pub sender: String,
    pub recipient: String,
    pub to: String,
    pub group: String,
    pub time: i64,
    pub text: String,
    pub kind: String,
    pub mention_me: bool,
    pub mention_all: bool,
    pub private: bool,
    pub mine: bool,
    pub withdrawn: bool,
    pub reply: String,
}
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct ImageRef {
    pub id: String,
    pub path: String,
}
impl Message {
    pub fn relevant(&self) -> bool {
        !self.withdrawn && (self.mine || self.mention_me || self.mention_all || self.private)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskEvidence {
    /// Messages that directly support the task's creation/current judgement.
    pub direct: Vec<String>,
    /// Bounded surrounding context, never used as completion proof.
    pub background: Vec<String>,
    /// Messages that explicitly support the completion decision.
    pub completion: Vec<String>,
}
impl TaskEvidence {
    pub fn all(&self) -> BTreeSet<String> {
        self.direct
            .iter()
            .chain(self.background.iter())
            .chain(self.completion.iter())
            .cloned()
            .collect()
    }
}
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TaskStep {
    pub id: String,
    pub title: String,
    pub status: String,
    pub depends_on: Vec<String>,
    pub waiting_for: String,
    pub next_step: String,
    pub confirmed: bool,
}
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Task {
    /// Legacy storage only; all tasks now use the global enabled tool registry.
    pub enabled_tool_ids: Vec<String>,
    /// Frozen versions used for the last AI decision, preserved in subsequent
    /// manual audit before/after records. No prompt or personal text here.
    pub decision_context: Value,
    /// Legacy storage only. Retained for history; no UI, AI schema or update path consumes it.
    pub steps: Vec<TaskStep>,
    pub suggested_project_id: String,
    pub suggested_project_reason: String,
    pub tags: Vec<crate::tags::Tag>,
    pub id: String,
    pub title: String,
    pub description: String,
    /// Compact, content-only summary sent in the initial task list.
    pub summary: String,
    /// Set only when the summary-relevant content changes.
    pub content_updated_at: i64,
    /// When the current summary was produced. Older than content means stale.
    pub summary_updated_at: i64,
    /// True when the summary is the deterministic truncated-content fallback.
    pub summary_fallback: bool,
    /// Direct/background/completion evidence is persisted separately.
    pub evidence: TaskEvidence,
    pub priority: String,
    pub environment: String,
    pub category: String,
    pub generation: String,
    pub generation_error: String,
    pub status: String,
    pub blocker: String,
    pub waiting_for: String,
    pub next_step: String,
    pub due: String,
    pub created: i64,
    pub updated: i64,
    pub archived: bool,
    pub archive_note: String,
    pub archived_at: i64,
    pub manual: bool,
    pub locked: bool,
    pub reason: String,
    pub merged_into: String,
    #[serde(default = "yes")]
    pub follow_deadline: bool,
    pub deadline_changed: i64,
    pub deferred_until: i64,
    pub schedule_id: String,
    pub scheduled_for: String,
}
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Schedule {
    pub tags: Vec<crate::tags::Tag>,
    pub id: String,
    pub title: String,
    pub description: String,
    pub priority: String,
    pub environment: String,
    pub category: String,
    pub frequency: String,
    pub weekdays: Vec<u32>,
    pub time: String,
    pub enabled: bool,
    pub created: i64,
    pub updated: i64,
    pub active_from: i64,
    pub last_generated_on: String,
}
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Audit {
    pub time: i64,
    pub kind: String,
    pub task_id: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub change_summary: String,
    pub before: Value,
    pub after: Value,
}
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Suggestion {
    pub decision: String,
    pub archived_at: i64,
    pub updated: i64,
    pub id: String,
    pub title: String,
    pub rule: String,
    pub evidence: Vec<usize>,
    pub active: bool,
    pub created: i64,
}
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MemoryEntry {
    pub id: String,
    /// Stable scope/key. The latest explicit correction in the same scope wins.
    pub key: String,
    pub text: String,
    /// Evidence identifiers from the archive batch (audit:N, message ids).
    pub source: Vec<String>,
    pub version: u64,
    pub created: i64,
    pub updated: i64,
}
/// Memory is private current state. Revision counters guard concurrent writes;
/// policy rollback never restores memory. Temporary memory belongs to incremental analysis; archive reads
/// it without replacing or clearing its contents.
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct AnalysisMemory {
    pub archived_entries: Vec<MemoryEntry>,
    pub archived_version: u64,
    pub temporary: String,
    pub temporary_version: u64,
    pub temporary_updated: i64,
}
impl AnalysisMemory {
    pub fn archived_text(&self) -> String {
        self.archived_entries
            .iter()
            .filter(|entry| !entry.text.trim().is_empty())
            .map(|entry| format!("- {}：{}", entry.key, entry.text.trim()))
            .collect::<Vec<_>>()
            .join("\n")
    }
}
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveAttempt {
    pub id: String,
    pub trigger: String,
    pub status: String,
    pub business_day: String,
    pub created: i64,
    pub started: i64,
    pub finished: i64,
    pub error: String,
    pub report_id: String,
    pub reused: bool,
    pub batches: usize,
    pub processed: usize,
    pub remaining: usize,
    pub archived_tasks: usize,
}
impl ArchiveAttempt {
    pub fn unfinished(&self) -> bool {
        ["queued", "running"].contains(&self.status.as_str())
    }
}
/// One immutable snapshot of everything an archive generation changed. A
/// rollback always appends a new group; it never rewrites history.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveGroup {
    pub id: String,
    pub created: i64,
    pub action: String,
    pub restored_from: String,
    pub attempt_id: String,
    pub report_id: String,
    pub reason: String,
    pub prompt: String,
    pub prompt_version: String,
    /// Only the rules that were active at generation time. Pending/archived
    /// suggestions stay in the single live list instead of being copied into
    /// every archive group (which grows quadratically over time).
    pub active_rules: Vec<Suggestion>,
    /// Rule revision at creation, used to reject a rollback that would erase
    /// manual rule changes made after the latest group.
    pub rule_version: u64,
    /// Active tag catalog at creation; tag version history is retained once in
    /// `tag_state`, not copied into every group.
    pub tag_groups: Vec<crate::tags::Group>,
    pub tag_version: String,
    pub archived_entries: Vec<MemoryEntry>,
    pub archived_version: u64,
}
impl Default for ArchiveGroup {
    fn default() -> Self {
        Self {
            id: String::new(),
            created: 0,
            action: String::new(),
            restored_from: String::new(),
            attempt_id: String::new(),
            report_id: String::new(),
            reason: String::new(),
            prompt: String::new(),
            prompt_version: String::new(),
            active_rules: Vec::new(),
            rule_version: 0,
            tag_groups: Vec::new(),
            tag_version: String::new(),
            archived_entries: Vec::new(),
            archived_version: 0,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct State {
    /// Monotonic local update clock and independent stage watermarks.
    pub message_updated_at: i64,
    pub filter_updated_at: i64,
    pub analysis_updated_at: i64,
    pub message_filter: crate::message_filter::FilterState,
    pub analysis_retries: BTreeMap<String, String>,
    /// Bounded conflict retries per message revision. A conflicting action keeps
    /// its messages pending for a few attempts; after the cap they are consumed
    /// with an explicit reason instead of retrying forever.
    pub analysis_retry_counts: BTreeMap<String, u32>,
    pub work_scheduler: crate::work_scheduler::SchedulerState,
    pub calendar_overrides: Vec<crate::holiday::ManualOverride>,
    pub morning_jobs: Vec<crate::calendar_service::MorningJob>,
    /// Explicit data-format version. Missing/unsupported values are handled by
    /// the managed-data cleanup flow, never by silently defaulting this struct.
    pub schema_version: u32,
    pub last_scan_finished_at: i64,
    pub analysis_pending_since: i64,
    pub last_analysis_at: i64,
    #[serde(default)]
    pub last_analysis_success_at: i64,
    pub ai_interrupted_at: i64,
    pub analysis_interrupted_at: i64,
    pub archive_ai_cursor: Option<usize>,
    pub archive_attempt_day: String,
    pub archive_attempts: Vec<ArchiveAttempt>,
    #[serde(with = "crate::archive_storage")]
    pub archive_versions: crate::shared::Shared<Vec<ArchiveGroup>>,
    pub analysis_memory: AnalysisMemory,
    /// A hard budget failure pauses automatic retries until a manual analysis
    /// succeeds. It never advances cursors or marks messages analyzed.
    pub auto_analysis_blocked: bool,
    pub auto_analysis_blocked_at: i64,
    pub auto_analysis_block_reason: String,
    pub deleted_messages: BTreeMap<String, i64>,
    pub advice_revisions: crate::shared::Shared<Vec<Value>>,
    pub archive_review_cursor: usize,
    pub archive_log_cursor: i64,
    pub tag_state: crate::tags::TagState,
    pub handling: crate::handling::HandlingState,
    pub tool_runs: crate::shared::Shared<Vec<crate::tool_calling::ToolRun>>,
    pub config: Config,
    pub messages: crate::shared::Shared<BTreeMap<String, Message>>,
    pub tasks: crate::shared::Shared<Vec<Task>>,
    pub task_numbers: BTreeMap<String, u64>,
    pub schedules: Vec<Schedule>,
    pub audit: crate::shared::Shared<Vec<Audit>>,
    pub suggestions: Vec<Suggestion>,
    pub rule_version: u64,
    pub last_scan: i64,
    pub last_error: String,
    pub analyzed: BTreeMap<String, String>,
    pub jobs: Vec<String>,
    pub usage_day: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub cached_reports: u64,
    pub unreported_calls: u64,
    pub reports: crate::shared::Shared<Vec<Value>>,
    pub retention_digest: Value,
    pub revision: u64,
    pub pending_scan: bool,
    pub pending_analysis: bool,
    pub pending_learning: bool,
    pub learning_cursor: usize,
    pub report_cursor: usize,
    pub prompt_versions: Vec<PromptVersion>,
    pub prompt_reviews: Vec<PromptReview>,
    pub chat_sources: Vec<ChatSource>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            message_updated_at: 0,
            filter_updated_at: 0,
            analysis_updated_at: 0,
            work_scheduler: Default::default(),
            message_filter: Default::default(),
            analysis_retries: BTreeMap::new(),
            analysis_retry_counts: BTreeMap::new(),
            calendar_overrides: vec![],
            morning_jobs: vec![],
            last_scan_finished_at: 0,
            analysis_pending_since: 0,
            last_analysis_at: 0,
            last_analysis_success_at: 0,
            ai_interrupted_at: 0,
            analysis_interrupted_at: 0,
            archive_ai_cursor: None,
            archive_attempt_day: String::new(),
            archive_attempts: Vec::new(),
            archive_versions: Default::default(),
            analysis_memory: AnalysisMemory::default(),
            auto_analysis_blocked: false,
            auto_analysis_blocked_at: 0,
            auto_analysis_block_reason: String::new(),
            deleted_messages: BTreeMap::new(),
            advice_revisions: Default::default(),
            archive_review_cursor: 0,
            archive_log_cursor: 0,
            tag_state: crate::tags::TagState::default(),
            handling: crate::handling::HandlingState::default(),
            tool_runs: Default::default(),
            config: Config::default(),
            messages: Default::default(),
            tasks: Default::default(),
            task_numbers: BTreeMap::new(),
            schedules: Vec::new(),
            audit: Default::default(),
            suggestions: Vec::new(),
            rule_version: 0,
            last_scan: 0,
            last_error: String::new(),
            analyzed: BTreeMap::new(),
            jobs: Vec::new(),
            usage_day: String::new(),
            calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cached_reports: 0,
            unreported_calls: 0,
            reports: Default::default(),
            retention_digest: Value::Null,
            revision: 0,
            pending_scan: false,
            pending_analysis: false,
            pending_learning: false,
            learning_cursor: 0,
            report_cursor: 0,
            prompt_versions: Vec::new(),
            prompt_reviews: Vec::new(),
            chat_sources: Vec::new(),
        }
    }
}
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PromptVersion {
    pub id: String,
    pub target: String,
    pub content: String,
    pub previous_id: String,
    pub action: String,
    pub reason: String,
    pub created: i64,
}
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct PromptReview {
    pub archived_at: i64,
    pub id: String,
    pub day: String,
    pub created: i64,
    pub status: String,
    pub base_version: String,
    pub base_prompt: String,
    pub evidence: Value,
    pub proposed_prompt: String,
    pub reason: String,
    pub evidence_ids: Vec<String>,
    pub error: String,
    pub last_attempt: i64,
}
#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ChatSource {
    pub id: String,
    pub plugin_id: String,
    pub account_key: String,
    pub name: String,
    pub enabled: bool,
    pub config: Value,
    pub cursor: Value,
    pub last_scan: i64,
    pub last_error: String,
    pub updated: i64,
}
pub fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
pub fn date() -> String {
    date_from(now())
}
pub fn date_from(at: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at)
        .map(|d| {
            (d + chrono::Duration::hours(8))
                .format("%Y-%m-%d")
                .to_string()
        })
        .unwrap_or_default()
}
pub fn stamp(t: i64) -> String {
    chrono::DateTime::from_timestamp_millis(t)
        .map(|d| {
            (d + chrono::Duration::hours(8))
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_default()
}
pub fn fingerprint(m: &Message) -> String {
    serde_json::json!({"text":m.text,"withdrawn":m.withdrawn}).to_string()
}
/// Older releases stored the whole Message. Read the recorded body, never the
/// live body, so upgrading cannot acknowledge a pending edit by accident.
pub fn canonical_fingerprint(stored: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(stored).ok()?;
    Some(
        serde_json::json!({
            "text": value.get("text")?.as_str()?,
            "withdrawn": value.get("withdrawn")?.as_bool()?,
        })
        .to_string(),
    )
}
pub fn fingerprint_matches(m: &Message, stored: Option<&String>) -> bool {
    stored.is_some_and(|stored| {
        *stored == fingerprint(m) || canonical_fingerprint(stored) == Some(fingerprint(m))
    })
}
pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn yes() -> bool {
    true
}
pub fn deadline_ms(due: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(due)
        .ok()
        .map(|d| d.timestamp_millis())
}
pub fn normalize_task(t: &mut Task) {
    if !["待处理", "已完成", "无需处理", "已取消"].contains(&t.status.as_str()) {
        t.status = "待处理".into();
    }
    crate::tags::sync_category_tags(t);
    normalize_evidence(t);
    refresh_summary(t);
}
/// Deterministic content-only fallback. The AI can improve the summary while
/// retaining the same content timestamps; generation failures keep this text.
pub fn summary_fallback(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return String::new();
    }
    let mut out = compact.chars().take(240).collect::<String>();
    if compact.chars().count() > 240 {
        out.push('…');
    }
    out
}
/// Summaries are never empty: fall back to the description, then the title,
/// then a fixed neutral placeholder for a task that has no content at all.
pub fn summary_content(t: &Task) -> String {
    let description = summary_fallback(&t.description);
    if !description.is_empty() {
        return description;
    }
    let title = summary_fallback(&t.title);
    if !title.is_empty() {
        return title;
    }
    "（未命名事项）".into()
}
pub fn refresh_summary(t: &mut Task) {
    if t.content_updated_at <= 0 {
        t.content_updated_at = t.updated.max(t.created);
    }
    if t.summary.trim().is_empty() || t.summary_updated_at < t.content_updated_at {
        t.summary = summary_content(t);
        t.summary_updated_at = t.content_updated_at.max(1);
        t.summary_fallback = true;
    }
}
/// Content changes invalidate the summary; the deterministic fallback keeps
/// the task list useful even when no model-generated summary is available.
pub fn mark_content_changed(t: &mut Task, at: i64) {
    t.content_updated_at = at.max(t.updated);
    t.summary_updated_at = 0;
    refresh_summary(t);
}
pub fn normalize_evidence(t: &mut Task) {
    for rows in [
        &mut t.evidence.direct,
        &mut t.evidence.background,
        &mut t.evidence.completion,
    ] {
        rows.retain(|id| !id.trim().is_empty());
        rows.sort();
        rows.dedup();
    }
}
pub fn merge_evidence(
    t: &mut Task,
    direct: &[String],
    background: &[String],
    completion: &[String],
) {
    for (target, rows) in [
        (&mut t.evidence.direct, direct),
        (&mut t.evidence.background, background),
        (&mut t.evidence.completion, completion),
    ] {
        for id in rows {
            if !id.is_empty() && !target.contains(id) {
                target.push(id.clone());
            }
        }
        target.sort();
        target.dedup();
    }
    normalize_evidence(t);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn audit_change_summary_is_optional_for_legacy_data_and_roundtrips() {
        let legacy = json!({"time":1,"kind":"ai_update","task_id":"synthetic","detail":"新进展","before":{},"after":{}});
        let mut audit: Audit = serde_json::from_value(legacy.clone()).unwrap();
        assert!(audit.change_summary.is_empty());
        assert_eq!(json!(audit), legacy);
        audit.change_summary = "阻塞已解除，继续验证".into();
        let restored: Audit = serde_json::from_value(json!(audit)).unwrap();
        assert_eq!(restored.change_summary, audit.change_summary);
    }
    #[test]
    fn message_revisions_ignore_all_metadata_including_replies_and_paths() {
        let original = Message {
            id: "m1".into(),
            text: "Original body".into(),
            ..Default::default()
        };
        let mut changed = Message {
            inserted_at: 1,
            updated_at: 2,
            images: vec![ImageRef {
                id: "image".into(),
                path: "cache/new.png".into(),
            }],
            source_id: "source".into(),
            source_name: "Renamed".into(),
            external_id: "external".into(),
            id: "different identity".into(),
            chat: "room".into(),
            from: "from".into(),
            sender: "renamed sender".into(),
            recipient: "recipient".into(),
            to: "to".into(),
            group: "renamed room".into(),
            time: now(),
            text: original.text.clone(),
            kind: "card".into(),
            mention_me: true,
            mention_all: true,
            private: true,
            mine: true,
            withdrawn: false,
            reply: "reference".into(),
        };
        assert_eq!(fingerprint(&original), fingerprint(&changed));
        let legacy = serde_json::to_string(&original).unwrap();
        assert!(fingerprint_matches(&changed, Some(&legacy)));
        changed.text.push_str(" edited");
        assert!(!fingerprint_matches(&changed, Some(&legacy)));
        changed.text = original.text.clone();
        changed.withdrawn = true;
        assert_ne!(fingerprint(&original), fingerprint(&changed));
        for invalid in ["not json", "{}", r#"{"text":"Original body"}"#] {
            assert!(!fingerprint_matches(&original, Some(&invalid.into())));
        }
    }
    #[test]
    fn default_policy() {
        let c = Config::default();
        assert_eq!(c.interval_minutes, 3);
        assert!(c.enabled);
        assert!(c.leaders.is_empty());
    }
    #[test]
    fn automation_effective_states_cascade_and_retired_backdrop_is_read_only_compatibility() {
        let mut c = Config::default();
        for scan in [false, true] {
            for analysis in [false, true] {
                for early in [false, true] {
                    c.enabled = scan;
                    c.auto_ai = analysis;
                    c.message_filter_enabled = early;
                    assert_eq!(c.automatic_analysis_enabled(), scan && analysis);
                    assert_eq!(c.early_analysis_enabled(), scan && analysis && early);
                    assert_eq!(
                        (c.enabled, c.auto_ai, c.message_filter_enabled),
                        (scan, analysis, early)
                    );
                }
            }
        }
        let legacy: Config =
            serde_json::from_value(serde_json::json!({"window_backdrop":false})).unwrap();
        assert!(!legacy.window_backdrop);
        assert!(serde_json::to_value(legacy)
            .unwrap()
            .get("window_backdrop")
            .is_none());
    }
    #[test]
    fn withdrawn_not_relevant() {
        let m = Message {
            mention_me: true,
            withdrawn: true,
            ..Default::default()
        };
        assert!(!m.relevant());
    }
    #[test]
    fn current_input_limit_is_kept_and_unknown_legacy_fields_are_rejected() {
        let config: Config = serde_json::from_str(r#"{"max_input_chars":70000}"#).unwrap();
        assert_eq!(config.max_input_chars, 70000);
        assert!(serde_json::from_str::<Config>(r#"{"analysis_batch_chars":9000}"#).is_err());
    }
    #[test]
    fn current_schema_keeps_config_and_derives_only_missing_summary_content() {
        let s: State = serde_json::from_value(json!({
            "schema_version": STATE_SCHEMA_VERSION,
            "config":{"max_input_chars":70000,"classify_prompt":"用户规则","auto_adopt_advice":false},
            "tasks":[{
                "id":"current","title":"保留标题","description":"用户已经编辑的描述",
                "status":"已完成","priority":"高","manual":true,
                "evidence":{"direct":["m1"]},"updated":123,"created":100
            }]
        }))
        .unwrap();
        assert_eq!(s.config.max_input_chars, 70000);
        assert_eq!(s.config.classify_prompt, "用户规则");
        assert!(!s.config.auto_adopt_prompt);
        assert!(!s.config.auto_adopt_advice);
        let mut t = s.tasks[0].clone();
        refresh_summary(&mut t);
        assert_eq!(t.title, "保留标题");
        assert_eq!(t.description, "用户已经编辑的描述");
        assert_eq!(t.status, "已完成");
        assert_eq!(t.priority, "高");
        assert_eq!(t.evidence.direct, vec!["m1"]);
        assert_eq!(t.summary, "用户已经编辑的描述");
        assert_eq!(t.content_updated_at, 123);
        let mut changed = t.clone();
        changed.description = "新的内容".into();
        mark_content_changed(&mut changed, 200);
        assert_eq!(changed.summary, "新的内容");
        assert_eq!(changed.summary_updated_at, 200);
        assert_eq!(changed.content_updated_at, 200);
        let empty = Task {
            title: "只有标题".into(),
            ..Default::default()
        };
        assert_eq!(summary_content(&empty), "只有标题");
        assert_eq!(summary_content(&Task::default()), "（未命名事项）");
    }
    #[test]
    fn missing_schema_version_and_unknown_legacy_fields_are_not_silently_defaulted() {
        let missing: Result<State, _> = serde_json::from_value(json!({"tasks":[]}));
        assert!(missing.is_ok()); // presence is enforced by the managed-data loader.
        let legacy: Result<Task, _> = serde_json::from_value(json!({"id":"old","sources":["m1"]}));
        assert!(legacy.is_err());
    }
    #[test]
    fn business_code_is_not_priority() {
        let mut t = Task {
            title: "项目代号A0".into(),
            ..Default::default()
        };
        normalize_task(&mut t);
        assert!(t.priority.is_empty());
        assert!(t.environment.is_empty());
        assert!(t.tags.is_empty());
    }
    #[test]
    fn approaching_deadline_raises_priority() {
        let t = Task {
            tags: vec![crate::tags::Tag {
                kind: "等级".into(),
                value: "稍后".into(),
            }],
            follow_deadline: true,
            status: "待处理".into(),
            due: "2026-10-15T17:30:00+08:00".into(),
            ..Default::default()
        };
        let at = deadline_ms("2026-10-15T16:30:00+08:00").unwrap();
        let catalog = vec![crate::tags::Group {
            display_priority: None,
            name: "等级".into(),
            values: ["立即", "优先", "常规", "稍后"].map(String::from).to_vec(),
        }];
        assert_eq!(
            crate::tags::effective_tags(&t, &catalog, "等级", at)[0].value,
            "优先"
        );
    }
}
