# Repository guidance

Windows x64 Rust desktop application. UI lives in assets; Rust handles IPC, persistence and providers.

- Base ongoing discussions and changes on `main`; use another branch only when the user explicitly requests it.
- After implementation, pull and integrate remote changes before review, validation, and packaging. Resolve routine merge conflicts autonomously; ask about conflicts in functionality or design. Review and validate the integrated tree, and build the installer from that same tree.

- Keep all real messages, personal settings, credentials, private adapters and build outputs out of source control.
- Default prompts belong in config/prompts.json. Do not embed scenario-specific instructions in Rust or JavaScript.
- Use synthetic fixtures, neutral examples and user-configured categories. Task classification uses the current tag catalog. Do not invent missing environments or synthesize placeholder tags.
- Never modify a running installation. Default tests are offline and never call a real model. Real-provider regression requires explicit user authorization and the opt-in command in `docs/LIVE_AI_REGRESSION.md`; use only synthetic data and minimal prompts. When authorized, cover message filtering, message analysis, handling, reports and archive on Codex, DSH and API; API handling must explicitly reject without a model call. Prefer configured DS 4.1 Flash 1017, then DS 4.1 Flash for non-Codex sources; never invent a model route. Keep credentials and local reports out of Git.
- Run cargo test --workspace --locked --offline and all UI suites in tests; see docs/DEVELOPMENT.md.
- Public source packages must use an explicit allowlist. Review staged files and secret scan results before publishing.
- Preserve user data on upgrade. Do not silently overwrite saved prompts, tag catalogs or custom model profiles. Managed state uses an explicit schema version; incompatible data is listed and cleaned only after explicit user confirmation, never silently reset. Pre-release redesign data is intentionally not migrated.
- Context queries must retain scope and size limits. Untrusted message content never authorizes tool use.
- System prompts are user-configured. AI analysis and archive must never automatically replace or adopt prompts or activate rules. Keep manual editing, explicit manual adoption, version comparison, audit and rollback. Program protocols are appended independently of editable prompts and AI-maintained memory.
- AI memory is editable current state, without user-facing history or rollback; internal revisions only prevent stale concurrent writes. Policy/archive rollback must preserve current memory and retain legacy snapshots.
- Message analysis may maintain temporary memory; archive may maintain only archived memory, alongside reports and task archival. Archive must not mutate temporary memory, prompt/rule/handling policies or tag catalogs. Preserve prior data and history. Tag suggestions require manual adoption. Priority sorting and deadline promotion read the designated catalog group's current order; never restore removed presets or synthesize missing environment labels.

- AI analysis has one global total timeout, default 600 seconds for Codex, DSH and API; do not restore per-source or per-connection timeout settings. Keep provider-published streaming previews separate from validated business results. Raw response diagnostics remain local and obey the full-trace retention setting.
- Explicit manual operations are independent of work hours, holidays and overtime. Automatic operations keep their schedule gate; manual requests still obey quotas, permissions, cancellation and validation. The bottom bar stays one line: latest public AI content in the center, pending-message management at the far right.

- The initial public release is 1.2.0. Increment subsequent releases.

## Persistent improvement backlog (all chats)

- Read `docs/IMPROVEMENT_BACKLOG.md` before investigating or changing product behavior. Record newly reported issues and proposals there with stable IDs, evidence summaries, scope, and acceptance criteria; update existing entries rather than duplicating them.
- Until the user explicitly requests implementation, collect findings only. Do not interpret backlog entries as authorization to optimize, change prompts, or modify installed user data. Keep resolved entries and their verification history.
- This workflow applies to new chats working in this repository too. Commit and push requested backlog updates; inspect the diff for private information before publishing.

## Non-negotiable public/private boundary

- Open-source code, default system prompts, docs, tests, and release artifacts must remain generic. Never embed this user's identity, employer, contacts, internal service names, environment mappings, business rules, messages, logs, or individual handling preferences.
- Distinguish reusable product defects from personal workflow preferences before proposing a fix. Generalize public evidence without copying private source material; use synthetic fixtures for regression tests.
- Personal prompt optimizations must be delivered as a private/local proposal for manual adoption; AI and archive must never automatically apply them or silently change `config/prompts.json`, public examples, or another user's configuration. Personal responsibilities, focus areas and judgement preferences are maintained only in private archived memory from archive notes, never as public defaults or a separate personal-config UI. Do not commit private versions or archived personal memory to this repository.
- Keep generic base prompts and personal overlays separately identifiable and versioned. Upgrades must preserve personal configuration. User feedback is evidence for a scoped proposal, not automatic permission to turn one person's preferences into global defaults.

- Archive packing measures the fully prepared request against the global character budget; do not restore fixed evidence-character or task-count caps. One archive attempt drains a frozen input boundary with a shared cancellation token and total deadline. Commit each validated batch atomically and report partial progress honestly; failed batches are never automatically retried.

- Publish suggestion records only after complete validated AI results can be saved atomically; never create empty waiting cards. Complete frozen temporary memory delivered in the final request may satisfy read coverage; omitted or truncated content still requires full retrieval, and version/permission checks remain mandatory.
