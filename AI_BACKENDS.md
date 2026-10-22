# AI 接入维护说明

当前可用驱动为 `codex`、`dsh` 和 `api`。`codex` 与 `dsh` 支持结构化分析和持久处置；`api` 只支持结构化分析，不执行一键处置。连接描述中不保存 API Key。

## 模块边界

- `src/ai.rs`：提示词组合、输出 schema、输入上限、取消检查、prepared 请求和调用入口。
- `src/ai_backend.rs`：连接解析、驱动选择、`Backend` 分析接口、`AgentSession` 处置接口。
- `src/ai_backend/codex.rs`、`src/codex_bridge.rs`：Codex CLI/app-server 参数、模型目录、登录状态、分析输出与事件解析。
- `src/desktop_bridge.rs`、`src/handling_desktop.rs`：桌面管道、独立来源登记、项目与上下文准备、创建、追加和只读同步；不恢复目标执行会话。`src/ai_backend/agent_approval.rs` 保留其他接入仍使用的审批范围校验。
- `src/ai_backend/dsh.rs`、`providers/acp/`：Harness 桌面 ACP 启动、受限 profile 就绪检查、分析提交、会话/审批/取消/历史适配。
- `src/ai_backend/api.rs`、`providers/api/`：OpenAI Chat Completions 兼容 HTTP、WinHTTP、严格 schema 校验、多模态只读工具循环和 `/models` 发现。
- `src/credentials.rs`：DPAPI 本地密钥或指定环境变量；密钥不进配置、状态、提示词 trace 或日志。
- `src/handling.rs`：事项关联、处置轮次状态、用量入账、学习和持久化。通过命名的会话操作调用适配器，不发送 RPC 方法。

## 连接与用途

`Config.codex` 保留为默认 Codex 连接的程序路径，空 `connection_id` 表示默认连接。`ai_connections` 保存附加连接的 `id/name/driver/executable/base_url/key_env`。`model_profiles` 有 `filter/analysis/handling/review` 四项，分别对应消息过滤、事项更新、AI 一键处置和归档分析。

`for_kind()` 仅在调用快照中解析用途；`selected_connection` 不序列化。部分字段为空的 profile 使用基础配置的对应值，不会静默变成空模型或空连接。缺失连接、重复 ID、未实现的驱动必须明确报错，不能回退到其他接入。任何用途解析到 `api` 的 `handling` 都拒绝执行。

处置 `Run.connection` 保存创建时的连接快照。后续操作使用该快照，不混用其他接入的会话编号。Codex 处置由本机已登录桌面执行；创建时提交用途模型和强度，后续仅在拾事配置明确变化时覆盖，避免还原桌面端修改。其他接入保持原有行为。

模型目录按连接分别加载；同一连接参数的读取合并。失败保留手动选择，过期请求不得覆盖正在编辑的表单。API 模型发现只调用 `GET /models`；失败时用户可以手填模型编号。检查连接使用当前表单值，保存设置前不会启动模型轮次。

## 驱动约定

1. `Backend` 实现分析、模型目录、参数校验和状态查询。只有真正支持处置的驱动才能开放处置；`api` 的 Agent 操作明确返回不支持。
2. 官方 API 端点必须是绝对 HTTPS，不能包含 userinfo、query 或 fragment；`/chat/completions` 和 `/models` 由客户端追加到调用方给定的路径前缀。
3. 请求使用应用实际测量并接受的同一 prompt/schema/tools。工具循环只执行调用方声明的只读工具；不支持的工具、参数越界、消息/工具/响应格式错误都失败，不用普通文本冒充结构化结果。只在服务端明确表示不支持 `response_format` 时做有限降级，不重放模糊失败或可能有副作用的调用。
4. 分析成功结果为业务 JSON 与用量。用量采用 `input_tokens/output_tokens/cached_input_tokens` 等现有字段；缺失计数表示未知，不能按零展示。工具结果中的图片转换为 Chat Completions 多模态 `image_url`，base64 不放进普通文本；限制 mime、base64 和总体积。
5. 处置结果沿用现有历史/审批 DTO。`start/read/resume` 返回 `thread`，`submit` 返回 `turn`，`history` 返回逆序 `data` 轮次数组；轮次保留 `id/status/startedAt/items/error`。适配器只保存在内存的历史不得在重启后覆盖应用已保存的轮次。
6. `UsageEvent` 含会话 ID、可选轮次 ID、原生 `notification` 及可选用量。Harness 处置保留受限会话；Codex 处置只连接桌面消息桥接，查询与入账继续去重。Codex 桌面未提供的用量保持未知；没有独立执行或交接回退。
7. 审批列表沿用 `id/method/params` 展示 DTO；回答由适配器转换，必须校验会话归属并展示协议原始范围，不扩大用户授予的权限。过期请求明确失败，不返回假成功。
8. 创建、提交和运行中插话的结果不确定时不得自动重试。独立分析必须使用新会话；处置继续原持久会话。补查次数、图片预算、取消、工具隔离和提交 schema 不能靠提示词替代。
9. Harness 分析和处置启动必须使用同一受限配置：分析使用 `--profile acp --patch <overlay>`、只读权限、空 `DSH_AGENTS_HOME` 和实际 `$DSH_HOME`；就绪检查读取版本和组合后的 `--dump-config`，校验活动插件 id/实现而不是只校验文件名。分析工具只允许 `read_context`/`read_image` 和 `submit_analysis`；没有 `submit_analysis` 接受结果或 schema 校验失败即整体失败。处置按用户已有权限展示审批，不支持的重命名、运行中插话等明确报错。
10. 不在普通日志记录提示词原文、模型思考、消息正文、密钥或会话凭据；只记录请求 id、长度、哈希、用量和失败类别。

## 流式、期限与本地诊断

- `Config.ai_timeout_seconds` 是三种结构化分析接入共用的总时限，默认 600 秒，可设 30—1800 秒；没有按连接或来源分别配置的期限。工具循环与格式纠正共享剩余期限，上游网关可能提前返回 504。
- API 连接的 `stream` 默认 true，可在高级选项明确关闭。WinHTTP 在 EOF 前逐块回调，SSE 解码保留正文、公开思考内容、工具参数及最终用量；DeepSeek 工具回合保留 `reasoning_content`，回放角色固定为 assistant。
- 已知只读工具参数校验失败允许一次纠正。全部参数通过验证前不执行该批工具；未知工具、权限拒绝及无进展不放行，不静默裁剪参数。
- 前端 AI 长操作先接受操作编号，再查询终态；查询不重发原操作。运行中进度仅展示实际收到的公开内容，完整业务结果通过校验后才提交。
- 普通活动日志仍只保存概况。开启“保留完整 AI 跟踪”时，原始响应和工具校验失败详情仅写本地调用目录，屏蔽当前 API 密钥及图片数据；工具失败单独保留，避免被有界事件流的截断隐藏。关闭时不保存这些正文。

## 验证

使用离线 Rust 测试及 `tests/ai-controls-ui.cjs`、`ui-regression.cjs`、`workflow-ui.cjs`、`inbox-pagination-ui.cjs`、`handling-ui.cjs`、`approvals-ui.cjs`、`tags-ui.cjs`、`ai-connections-ui.cjs`、`work-state-ui.cjs`。API/ACP 使用 mock HTTP、mock ACP server 和合成数据；处置测试使用子进程模拟 app-server，覆盖协议、审批、插话、用量去重、配置切换后旧连接绑定和重启持久化，不调用真实模型。真实供应商兼容性、缓存命中和安装版 Harness 运行不作为已验证结论。

## 1.1.2 进度与归档契约

- 手动用途不受工作时段、假期或日历状态限制；自动请求保留原时段门禁。配额、权限和结果校验独立执行。
- Codex `exec --json` 的条目快照不等同于逐字增量。前台元数据操作在独立工作线程等待本次线程可读后执行，只有明确的 rollout 暂未出现会有界等待；不重发模型请求。前台失败不改变分析结果。
- 在 CLI 返回本次线程的本地路径时，仅监看该新线程公开的 `event_msg.agent_reasoning/agent_message` 文本，分块读取且限制记录、队列和预览大小；忽略原始 reasoning、加密项和其他会话。路径不可用时继续显示实际 JSONL 文本与阶段，不声称供应商已公开思考。
- 底部中间只显示最新公开消息行或实际阶段，待分析入口在最右，窄屏单行截断。阶段更新不冒充供应商文本到达。
- 归档 payload 明确给出 `archive_memory_sources`，允许从本批操作记录整理长期经验；`archive_note_sources` 提供备注补充。动态 schema 与保存校验使用相同的冻结来源，没有有效操作来源时不生成记忆变更。保留用户已有提示词及其他权限边界；校验错误仍整批拒绝，不静默丢弃、不自动重试。
- `ai_success(kind=archive)` 仅指模型响应返回；`archive_result` 才表示业务已原子提交，`archive_failed` 记录校验或保存失败；最终状态进入 AI/归档及异常筛选。

## 1.1.3 归档预算与前台容错

归档使用完整请求（提示词、记忆、事项摘要、增量、输出格式和工具定义）的实际字符数，与单次输入上限比较；移除固定 9000 字符和事项条数导致的提前切分，相关事项不在两组摘要中重复发送。只有确实超限时才分批，同一次尝试处理开始时冻结的材料；后续新到材料留给下次。整轮共用总期限与取消控制，各模型请求独立记录跟踪与用量；每批重新检查配额和自动归档授权。失败保留前面已提交批次，停止后续调用，未处理游标保留。

Codex exec 与辅助 app-server 的加载状态独立。读取明确采用 includeTurns=false；已知暂未就绪可用 thread/list 的 exec 来源摘要查找同一个编号，不 resume 活跃写入会话，不发送新轮次。系统打开会话不依赖可选命名/项目元数据成功。元数据读取失败仍尝试精确会话链接，保留诊断。临时记忆校验快照不参与公共线格式压缩；首次空原版本必须保留，缺失原版本仍拒绝。

## 1.1.4 建议与记忆交付

处置学习只接受本次有结论或人工纠正的有界材料，不开放通用消息补查；材料充分性单独返回，建议仅由用户手动采用。提示词重新评估在完整输出校验后一次保存，不持久化生成占位。完整临时记忆若已进入同一请求最终提示词，私有快照记录该版本的交付凭据；缺失或截断仍须完整补查，所有来源继续共用版本、权限及保存校验。
