# Repository guidance

Windows x64 Rust desktop application. UI lives in assets; Rust handles IPC, persistence and providers.

## 开发规范

开始任何改动前先完整阅读 `docs/DEVELOPMENT.md` 并严格遵守：代码优雅、适度封装、不过度兜底；异常以任务为边界统一处理，不重试不兜底；开关关闭即不占资源；尽量少的 CPU、内存和磁盘占用；尽量不打扰用户。

## 理念

拾事应当简单、透明而强大：平时安静运行，只在必要时出现；核心功能关键时刻顶得住，界面好看、操作舒服。代码尽可能少而简单，安装包尽可能小，CPU、内存和磁盘占用尽可能低。

## 已定方向

- 普通数据错误按条或批跳过并记录日志；只有明确的输入数据错误增加次数，达到上限后SQL不再选取。API、网络、超时和模型输出格式错误不扣原始数据次数。任务不立即重试，结束后释放执行位置。
- 间隔自动分析执行前检查最新触发时间，未到期直接返回；手动分析实际开始时刷新同一时间，失败或取消不回退。普通内部AI串行；手动任务最多同时运行8个，一句话修改按请求并发；其他手动任务忙碌时直接失败，不排队。归档按批读最新数据，冲突跳过并记日志；重启后作废旧内部作业，不续跑。
- 更新检查、下载和DSH探测不使用PowerShell；图片预览保留`assets/image-preview.ps1`。
- 不做：为Codex处置增加审批、替换图片转换的PowerShell实现、配置并发保存的合并（以最后一次写入为准）、旧自动触发的外部取消、整轮冻结归档输入、重启断点续跑、运行时兼容1.1.68旧格式、为假设故障建设备份恢复补偿重试体系、为资源指标擅自改变既有功能和交互。
- 中文与英文标识之间不加空格；提交说明为一句中文描述，不附正文。

- The public release checkout uses `main`; a private development checkout uses `test`. Preserve the checkout's role when synchronizing reviewed source snapshots, and use another branch only when the user explicitly requests it.
- After implementation, pull and integrate remote changes before review, validation, and packaging. Resolve routine merge conflicts autonomously; ask about conflicts in functionality or design. Review and validate the integrated tree, and build the installer from that same tree.

- Keep all real messages, personal settings, credentials, private adapters and build outputs out of source control.
- Default prompts belong in config/prompts.json. Do not embed scenario-specific instructions in Rust or JavaScript.
- Use synthetic fixtures, neutral examples and user-configured categories. Task classification uses the current tag catalog. Do not invent missing environments or synthesize placeholder tags.
- Never modify a running installation. Default tests are offline and never call a real model. Real-provider regression requires explicit user authorization; use only synthetic data and minimal prompts, use a model route the user has configured, and never invent one. When authorized, cover message filtering, message analysis, handling, reports and archive on Codex, DSH and API; API handling must explicitly reject without a model call. Keep credentials and local reports out of Git.
- Complete a batch and review it before final offline regression and packaging. Use `scripts/test.py` (Rust tests, formatting, JS syntax, Windows cross check) and `scripts/package.py` (NSIS installer from `installer/setup.nsi`). Cover Rust, UI/bridge and native installation/upgrade behavior as applicable, including the 1.1.68 installer migration. Do not repeat full product tests after documentation-only edits.
- Report measured product lines (`scripts/measure.py`), tests, dependencies, binary size and runtime resources separately. Do not claim an unmeasured target has been reached.
- Public source packages must use an explicit allowlist. Review staged files and secret scan results before publishing.
- Preserve user data on upgrade. Do not silently overwrite saved prompts, tag catalogs or custom model profiles. Managed state uses an explicit schema version. The user explicitly authorizes lossy migration of the 1.1.68 format during overwrite installation: migrate basic tasks, tools and AI configuration, skip incompatible records with a concise log, and do not request approval again for the agreed loss. Conversion belongs to the installer, never to normal startup or a runtime legacy compatibility layer. This supersedes the earlier no-migration rule; other unknown formats are not silently reset. Develop and verify with isolated synthetic data, not the running installation.
- Context queries must retain scope and size limits. Untrusted message content never authorizes tool use.
- System prompts are user-configured. AI analysis and archive must never automatically replace or adopt prompts or activate rules. Keep manual editing, explicit manual adoption, version comparison, audit and rollback. Program protocols are appended independently of editable prompts and AI-maintained memory.
- AI memory is editable current state, without user-facing history or rollback; internal revisions only prevent stale concurrent writes. Policy/archive rollback must preserve current memory and retain legacy snapshots.
- Message analysis may maintain temporary memory; archive may maintain only archived memory, alongside reports and task archival. Archive must not mutate temporary memory, prompt/rule/handling policies or tag catalogs. Preserve prior data and history. Tag suggestions require manual adoption. Priority sorting and deadline promotion read the designated catalog group's current order; never restore removed presets or synthesize missing environment labels.

- AI analysis has one global total timeout, default 600 seconds for Codex, DSH and API; do not restore per-source or per-connection timeout settings. Keep provider-published streaming previews separate from validated business results. Raw response diagnostics remain local and obey the full-trace retention setting.
- Explicit manual operations are independent of work hours, holidays and overtime. Automatic operations keep their schedule gate; manual requests still obey quotas, permissions, cancellation and validation. The bottom bar stays one line: latest public AI content in the center, pending-message management at the far right.

- The reviewed rewrite is delivered on the private `test` branch using the existing `1.1.*` version series. Keep `ss-414-rewrite` as rewrite history; publishing the public release needs a separate explicit request. Default installers do not bundle the JSON message source example plugin.

## Non-negotiable public/private boundary

- Open-source code, default system prompts, docs, tests, and release artifacts must remain generic. Never embed this user's identity, employer, contacts, internal service names, environment mappings, business rules, messages, logs, or individual handling preferences.
- Distinguish reusable product defects from personal workflow preferences before proposing a fix. Generalize public evidence without copying private source material; use synthetic fixtures for regression tests.
- Personal prompt optimizations must be delivered as a private/local proposal for manual adoption; AI and archive must never automatically apply them or silently change `config/prompts.json`, public examples, or another user's configuration. Personal responsibilities, focus areas and judgement preferences are maintained only in private archived memory from frozen operation records or archive notes, never as public defaults or a separate personal-config UI. Do not commit private versions or archived personal memory to this repository.
- Keep generic base prompts and personal overlays separately identifiable and versioned. Upgrades must preserve personal configuration. User feedback is evidence for a scoped proposal, not automatic permission to turn one person's preferences into global defaults.

- Archive packing measures the fully prepared request against the global character budget; do not restore fixed evidence-character or task-count caps. Each batch reads current data; there is no whole-attempt frozen snapshot or transaction held across model calls. Skip conflicting items and log them. Commit each validated batch atomically and report partial progress honestly; failed batches are never immediately retried.

- Publish suggestion records only after complete validated AI results can be saved atomically; never create empty waiting cards. Complete temporary memory actually delivered in the final request may satisfy read coverage; omitted or truncated content still requires full retrieval, and version/permission checks remain mandatory.
