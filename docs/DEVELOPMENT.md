# 开发与验证

## 环境

- Windows x64、Rust stable 2021 edition、Cargo。
- 资源脚本优先调用 GNU `windres`；不可用时回退到 Windows SDK 的 `rc.exe`（检查 `RC`、`WindowsSdkDir` 和标准 SDK 目录）。已验证 GNU 路径；MSVC/rc 回退只按构建脚本中的失败可见行为维护，不宣称所有 MSVC 组合已验证。
- Microsoft Edge WebView2 Runtime。
- 构建安装器需要 NSIS 3；运行 UI 回归需要 Node.js、Playwright 和本机 Edge。

此仓库没有个人机器路径；通过环境变量选择本机工具。CLI 协议测试使用模拟 app-server，不消费真实模型额度。

## Rust

```powershell
cargo fmt --all -- --check
cargo test --workspace --locked --offline
cargo build --package shishi --release --locked --offline
```

依赖已缓存时可加 `--offline`。若默认 Rust target 不是 GNU，请明确选择匹配的 GNU toolchain/target 并准备 linker 和 windres；不要把未经测试的 MSVC 组合视为已验证。

## 界面结构

界面源码在 `assets/ui/`：`index.html` 是页面骨架，`styles/` 依次为设计 token、布局规则和设计层，`scripts/` 按页面拆分并在同一全局作用域中按 `manifest.txt` 顺序拼接。新增文件须登记到 `manifest.txt`；`build.rs`、`--ui-dir` 和 UI 测试（`tests/ui-bundle.cjs`）都按同一清单组装。颜色只引用 `styles/tokens.css` 中的 token。

只改界面时，可运行已构建的程序并加 `--ui-dir <仓库>/assets/ui`，重启即可看到改动，无需重编 Rust；仍须配合 `--root <isolated-directory>` 使用隔离数据目录。

## UI 回归

在仓库以外准备 Node 依赖目录，避免将下载的包纳入源码：

```powershell
npm install --prefix <dependency-directory> playwright
$env:NODE_PATH='<dependency-directory>\node_modules'
node tests/ai-controls-ui.cjs
node tests/ui-regression.cjs
node tests/workflow-ui.cjs
node tests/inbox-pagination-ui.cjs
node tests/handling-ui.cjs
node tests/approvals-ui.cjs
node tests/tags-ui.cjs
node tests/ai-connections-ui.cjs
node tests/compact-ui.cjs
node tests/work-state-ui.cjs
node tests/state-sync-ui.cjs
node tests/palette-ui.cjs
node tests/logs-ui.cjs
node tests/log-purpose-ui.cjs
node tests/settings-autosave-ui.cjs
node tests/unified-tags-ui.cjs
node tests/backdrop-ui.cjs
node tests/analysis-display-ui.cjs
node tests/ai-stream-ui.cjs
node tests/layout-stability-ui.cjs
node tests/experience-ui.cjs
node tests/motion-ui.cjs
node tests/backlog-ui.cjs
node tests/approval-desktop-ui.cjs
node tests/handling-reading-ui.cjs
node tests/reading-motion-ui.cjs
node tests/todo-refinements-ui.cjs
node tests/connection-followup-ui.cjs
node tests/storage-settings-ui.cjs
node tests/about-history-ui.cjs
node tests/desktop-handling-ui.cjs
node tests/update-schedule-ui.cjs
node tests/pending-fixes-ui.cjs
node tests/supplement-fixes-ui.cjs
node tests/reading-modal-fixes-ui.cjs
node tests/trace-dialog-ui.cjs
node tests/empty-purpose-ui.cjs
node tests/ai-metrics-ui.cjs
node tests/settings-automation-ui.cjs
```

Codex 处置通过桌面消息管道创建、追加和读取，测试不得重新启用独立 app-server 持有目标会话。`desktop_bridge` 的离线用例使用合成 Windows 管道，`handling_desktop` 验证状态、双端输入、草稿、结束同步和迟到结果。`--desktop-bridge-check --root <隔离目录> --output <报告>` 仅检查已登录桌面和本程序的空接入记录，不发送模型提示词；首次会建立零模型登记。逐字流式另行验收，整条消息快照不能代替文本增量订阅。

`work-state-ui.cjs` 覆盖工作状态/加班、旧步骤不再展示且历史保留、统一消息依据的搜索/排序和归档版本比较；`state-sync-ui.cjs` 模拟只返回所请求分区的后端，覆盖按页拉取、空闲无轮询和推送刷新。它们和其余套件一样使用模拟 IPC 与合成数据。

UI 测试使用合成数据和模拟 IPC，浏览器无头运行，输出写入系统临时目录；不会控制正在运行的拾事。某些测试明确标记 ignored，是进程模拟入口或需要显式数据的离线实验，不应以真实资料执行后把产物提交。

`about-history-ui.cjs` 覆盖关于我们、三个外部入口、标题行更新控件、固定发布于标签及旧安装/更新时间隐藏、检查加载与重复点击、准备完成后的确认与稍后处理、固定事项编号、变更记录查询与排序、空依据禁用、长记录底部收起、宽窄窗口的数据目录布局、系统日志搜索请求和记忆副标题。原生更新助手的隔离验证边界见 [版本更新](UPDATES.md)。

`update-schedule-ui.cjs` 使用合成安装包状态和可控时钟验证每日提醒、立即／稍后选择、凌晨独立安装请求、开关关闭、请求过期与失败去重。Rust 时钟用例另验证边界跨越、日期持久去重、启动／休眠不补执行以及缓存缺失或损坏；不修改系统时间，不下载真实发布包。

`pending-fixes-ui.cjs` 使用合成消息及可控时钟验证扫描新增／更新分项、历史日志兼容、来源读取失败、发布于标签、无更新按钮完整保持 30 秒及跨页恢复、标签优先级常驻双列与失焦保存、结束提示对齐，以及进展内层详情展开收起后的最新位置和控件可见性。Rust 扫描用例覆盖重复、元数据、正文编辑、撤回、删除／过期、配置变化及保存失败。

`ai-metrics-ui.cjs` 使用可控时钟验证 API 每秒有效增量优先、DSH 完成步骤均值兜底和终态统一均值、首个公开内容前隐藏、稳定零值及请求隔离。API 离线流式用例另覆盖分片计数、多轮工具累计、逐步首个输出至组装结束计时、缺失用量与终端总量不进入实时采样；不使用字符估算，不把等待、工具或本地校验时间计入生成速度。

插件恶意路径 ZIP 是刻意构造的测试夹具，可用 `python tests/make_plugin_fixtures.py` 重新生成；不要把其中字节当成可执行插件发布。

## 安装包

```powershell
./build.ps1 -NsisPath <path-to-makensis.exe>
```

脚本构建主程序，定位 WebView2Loader.dll，调用 NSIS 并输出安装器 SHA-256。安装器仅包含主程序、运行依赖和安装说明，不嵌入 `data/` 或私人插件。不要直接覆盖正在使用的安装目录进行验证。

本地调试可使用 `target/release/shishi.exe --root <isolated-directory>`；仅使用全新隔离目录。仓库首发不承诺读取早期开发版本的数据。

Mica 原生检查：`./tests/backdrop-native.ps1 -Binary <shishi.exe> -OutputDirectory <isolated-output-directory>`。只启动独立合成目录中的自检窗口，关闭后台扫描与模型调用，截取该进程的客户区并缩放检查，随后执行关闭到托盘／退出验证。`--ui-smoke-mica` 验证本机实际支持的 Mica 路径，`--ui-smoke-mica-fallback` 注入初始化失败、验证深色回退；报告明确记录本机是否真正启用 Mica。不改系统主题、透明开关或运行中的安装。截图不能替代其他 Windows 版本、系统节电和显示缩放组合的现场验证。

## 变更检查

`python tests/check-release.py` 使用临时合成文件验证发布检查的公共规则、仓库外私有词典、字面匹配和失败时的脱敏行为，不读取实际用户词典。

提示词在 `config/prompts.json` 修改，协议和 schema 的实际约束仍由代码实现。变更默认配置时同时更新 README 和相关边界测试。提交前运行 `python scripts/check_release.py`；该脚本同时扫描已跟踪文件和未忽略的未跟踪文件，因此新增 `providers/`、`src/` 源文件在 `git add` 之前也会经过 allowlist 和隐私规则。生成源码包使用 `python scripts/package_source.py`。禁止 `git add .` 不经审查直接上传运行目录。

`compact-ui.cjs` 覆盖紧凑布局、帮助浮窗、事项附加字段、提示词独立编辑、收件箱只刷新与窄屏；`ai-connections-ui.cjs` 覆盖接入草稿、检测竞态、模型目录与保存/取消。

运行全部 UI 回归时也包含 `node tests/state-sync-ui.cjs`；`node scripts/frontend.cjs build/frontend-preview.html` 使用同一 manifest 生成预览。DSH 原始失败帧诊断仅在显式设置 `SHISHI_ACP_DIAGNOSTICS=1` 时输出前 240 字符，勿提交本地诊断正文。

DSH 检测对缺失 ACP profile 提供当前启动器及 DSH_HOME 对应的 PowerShell 初始化命令；不会自动运行命令或重置已有 profile。不可读、损坏和受限补丁缺失分别报错。初始化语义参考 [官方 CLI 行为说明](https://github.com/deepseek-ai/deepseek-harness/blob/master/apps/cli/reference/README.md)；初始化后必须重新通过版本和受限配置检查。回归使用合成文件及模拟 ACP，不修改真实 DSH 用户目录。

DSH 缺少 ACP profile 时可显式确认初始化：`dsh_initialize_acp` 要求 confirmed=true，使用所选启动器、同一 DSH_HOME 及受限补丁执行配置导出，先校验版本、后复核就绪；不运行插件安装命令，不重置已有 profile。初始化与模型目录测试使用临时模拟启动器。模型目录保留候选/已选两组，支持全选、反选和只保存已选条目。

日志页通过 activity_query 按上海日期和类型读取 JSONL（含轮转文件），筛选后分页，不依赖实时快照的最后 60 条。查询只读，按页返回并保留审计下标；损坏行单独计数，文件读取失败明确返回错误。

`analysis-display-ui.cjs` 覆盖显示优先级与精简标签、成功分析时间、固定请求来源、DSH 内部供应商、可见模型全选／反选与结构化请求比较。`SHISHI_UI_BROWSER` 可为 `ai-connections-ui.cjs` 指定 Chrome 可执行文件，独立复核相同保存失败与焦点流程。HTTP 回归含约 32 秒的本地慢响应，验证 WinHTTP 不再沿用默认接收期限；不访问真实模型。

真实接入回归必须遵循 [固定矩阵与合成数据规范](LIVE_AI_REGRESSION.md)，仅在用户明确授权后运行。


1.1.3 离线回归补充：归档测试以实际 prepared 输入预算验证大于旧固定阈值仍单批处理，模拟手动/持久化 claim 自动归档跨批、批次间新到材料、后批失败/取消/总时限，确认前批提交不回滚且不重试。临时记忆测试从空状态经过完整请求准备/压缩、验证、保存/重载，再完整读取并更新下一版。`work-state-ui.cjs` 覆盖浮层 1280/1000/760/360 宽度、外部点击/Escape、日期校正、已归档标记和部分归档状态。Codex 协议测试覆盖未加载摘要发现、可选元数据失败仍打开、错误编号拒绝；原生系统唤起只允许使用合成会话、不发送模型轮次，并在验收后清理。

1.1.4 离线回归补充：建议重新评估覆盖处理中磁盘无新占位、取消/非法输出/保存失败/并发版本保护及完成结果重启保留。处置学习覆盖首轮结构化背景、实际截断、空中断轮次不调用模型、材料不足状态及材料更新后的手动重评估。临时记忆以实际 prepared 请求完成首次写入、完整内联续写及保存重载，另覆盖省略/截断、完整补查和旧版本拒绝。`workflow-ui.cjs` 覆盖旧空卡片不显示、不计待确认和完整结果统计。

1.1.5 离线回归补充：消息指纹只比较正文和撤回状态，兼容已保存的完整消息指纹；覆盖所有元数据变化、旧正文编辑、新回复身份及重启。过滤决定在持久化预留完整请求时消费，消费匹配原决定版本，不由成功路径清除；完整请求接收的消息版本同时记入已过滤记录，避免过滤单批之外的同一积压再触发。过滤表只保留回看范围内仍待分析的有效版本，不能按固定条数驱逐后重复过滤；未处理消息不标记已分析。覆盖保存失败、额度拒绝、大于 500 条的完整批次及新版本保护。`work-state-ui.cjs` 验证消息依据排序、搜索和计数、归档成功五分钟/跨日隐藏，失败仍可见；`inbox-pagination-ui.cjs` 验证全查询排序与分页；`compact-ui.cjs` 验证数据设置整合及路径控件。

更新时间游标回归：`message_progress.rs` 覆盖原始时间/插入时间保持、显示元数据不推进、本体变化重新入队、前批成功而后批失败、连续前缀及异常空洞。`sources.rs` 用合成扫描验证新增/编辑/撤回和保存失败的更新时间语义。正常进度使用过滤/分析更新时间游标；旧版本表只辅助兼容及空洞，归档继续沿用既有逐批事件游标。

1.1.6 离线回归补充：实际后端快照包含 evidence_count 等展示字段时，人工保存及 AI 修改均通过统一转换边界；未知业务字段仍拒绝，AI 修改用零额度在真实预留边界阻断，不调用模型。新过滤决定绕过上一轮常规冷却，消费后不重复，常规到期规则保持。进度游标记录最新已确认版本，待处理空洞显式保留在既有版本表中；覆盖范围外旧消息不阻塞、扩大回看仍可找到、重启保留和分析期间新到消息不被旧批次覆盖。

1.1.7 离线回归补充：`experience-ui.cjs` 覆盖交接前打开客户端、长结论可视收起与刷新后阅读位置、控制权独立于旧轮次状态、两类记忆编辑及冲突草稿、独立说明与完整插件协议复制、进度 DOM 身份和上翻后暂停跟随。Rust 覆盖固定 MCP 的显式凭证、跨事项隔离与到期、只读桌面记录观察、人工编辑与 AI/归档的并发保护，以及归档回滚保留当前记忆。固定 MCP 配置和交接说明仅通过模拟 app-server 验证；不使用真实用户配置或提交模型轮次。

原生固定 MCP 检查：`python tests/context-native.py --binary <shishi.exe> --codex <codex.exe> --output <isolated-output-directory>`。仅创建合成事项和独立 Codex 配置目录，测试进程重启、事项凭证隔离及真实接口发现，不执行 turn/start 或真实模型请求，也不写实际用户的 Codex 配置。

`motion-ui.cjs` 覆盖快速过渡上限、流式中文与 emoji、稳定节点和光标、短快照校准、阅读跟随意图、Enter/IME/Ctrl+Enter、失败草稿、固定权限/进度弹窗及响应后失败标题。全程合成数据，不调用真实模型。

1.1.9 离线回归补充：`backlog-ui.cjs` 验证紧凑处置阅读区、顶部可收起的技术详情、正常与来源已有损坏字符保留、内外层滚动优先级及上下边界、追问增高/收缩、依据默认倒序、扫描状态互斥、终止后新会话以及日志失败恢复/北京时间跨日。`approval-desktop-ui.cjs` 与主界面共用审批渲染，验证关闭不拒绝、回答失败保留和重复请求编号跨 run 隔离。Rust 覆盖停止标记持久化、启动锁占用、缓存等待、迟到事件、初始/收尾保存失败、关闭失败重试、新会话及上下文凭证撤销。

原生审批自检：`shishi.exe --ui-smoke-approval --root <new-isolated-directory>`。创建内存中的合成审批，仅检查独立 WebView、工作区位置、滚动/按钮、关闭/解决后的隐藏以及主窗口托盘生命周期；不执行命令或调用模型。自检要求显式指定全新隔离目录，详细结果写入该目录的 `data/ui-smoke.json`。

1.1.10 离线回归补充：`handling-reading-ui.cjs` 验证实时进度和结论共用底部展开收起、展开正文与长代码块均无高度上限、主界面双向滚动、三种窗口尺寸、刷新及流式追加保留节点和阅读位置、轮次隔离、短内容隐藏按钮及后续增长重新显示。既有体验、动效和待办回归按新的整框阅读规则检查，审批弹框继续保持独立滚动边界。

1.1.11 离线回归补充：阅读框收起时支持框内滚动，展开时将原内层偏移转入主界面，保留阅读位置与换行宽度，前后内容完整保留。检查展开底栏悬浮、末行不被遮挡、收起后的可达性及两框上下边界衔接；保留刷新、追加与轮次状态验证。

1.1.12 离线回归补充：`reading-motion-ui.cjs` 按逐帧文字坐标验证中心锚点、真实高度过渡、无滚动往返位置、刷新与连续反向、不可取消的连续边界输入；`todo-refinements-ui.cjs` 覆盖已交接会话仅结束本地关联后新建、本地已完成会话结束、范围空选与全局清除、归档和搜索边界，以及三种窗口下日志列表独立滚动与错误操作保留。Rust 使用模拟执行端验证结束关联不停止远端、原子保存、重载保持和旧事件隔离。

补充验收：`supplement-fixes-ui.cjs` 覆盖无多余外层标题、耗时行删除、键盘发送及整行删除。`reading-modal-fixes-ui.cjs` 覆盖记录标题在内外阅读区的吸顶位置、所属记录边界、文字宽度，以及原层弹窗退场的尺寸、模糊、滚动锁和快速开关。`powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -File tests/update-download-metadata.ps1` 只加载下载脚本的纯分类函数，用合成 HTTP 响应离线验证限流与错误分类，不执行下载器。

`trace-dialog-ui.cjs` 使用合成调用记录验证两个主阅读框的紧凑高度、四种窗口下默认无外层滚动、详情展开后滚动，以及软件外失焦保留和软件内遮罩关闭；不读取真实提示词。

`empty-purpose-ui.cjs` 验证四组用途留空、显式选模型、空值保存、删除源后同步清空、返回设置仍为空、错误不弹窗，以及四种窗口宽度下模型行样式一致。Rust 覆盖新安装默认值、旧配置兼容、全部用途拒绝前置、错误去重、额度与业务进度保留，以及删除与用途清空的原子保存。

`ai-metrics-ui.cjs` 验证用量行、底部流水末尾和实时弹窗的每秒输出增量与完成后平均速度，首个公开内容延迟、完成后保留、旧请求隔离、未知／部分／零用量及零耗时边界；核对弹窗双行指标和三种宽度下底栏单行布局。延迟不将阶段或工具状态当作首个公开内容，速度不从字符数估算。


状态维护约定：`engine/persistence.rs` 负责持久化提交，编辑、页面投影与报表分别放在 `engine/` 的对应模块；原回归组独立保存于 `engine/tests.rs`。业务共享状态采用候选状态保存成功后发布，公开流式预览、取消与运行失败反馈不冒充业务提交。报表状态是提交依据，旁路报表文件失败记录日志。

日志由 `log_writer.rs` 的有界队列和一个消费线程独占追加与轮转；查询、数据迁移和正常退出使用屏障排空此前提交的记录。各生产线程不直接操作日志文件。处置文本预览绑定持久化版本，审批、用量和生命周期变化立即提交并吸收此前预览。预览文字在意外退出前尚未进入业务保存时可能缺少末段；已有保存的步骤与终态继续恢复。

`ui_refresh.rs` 在业务 revision 未变化时只返回运行态；前端保留原页面、草稿和光标，忽略旧页面的迟到响应。`runtime-refresh-ui.cjs` 验证这些边界。JS 按清单顺序保留共享作用域，模型路由、接入、消息、合并、工作状态和帮助已分文件，暂不引入新的状态框架。

通用子进程 stdout 上限 64 MiB、stderr 8 MiB、单行 8 MiB；每读取 256 KiB 返回取消及期限检查，最终排空也执行检查。更新缓存仅回收超过 24 小时且命名可确认属于程序的文件，保留当前已验证包、被占用的助手及任何未知内容。

构建脚本从 `cargo metadata --locked --offline` 的实际依赖定位 x64 WebView2 加载器，输出 `Shishi-Build-Metrics-<version>.json`，包含安装包、主程序、嵌入 UI 大小与加载器版本／摘要。不以源码行数或删除未使用 Rust 函数推断产物一定缩小。
