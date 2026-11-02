# 拾事 CLI

安装包包含 `shishi.exe` 和同目录的 `ss.exe`。CLI 通过本机接口把请求交给正在运行的拾事，复用页面的业务函数、校验、锁和保存流程。CLI 不直接打开或修改业务数据库。

需要让 agent 操作拾事时，在「使用说明」复制 ShiShi CLI Skill，或让它读取安装目录的 `skills/shishi-cli/SKILL.md`。技能包含命令发现、查询、带版本的修改、异步任务查询及失败核对方式；读取技能本身不会执行操作。

命令默认返回 UTF-8 JSON：成功时 `ok` 为 `true`、结果位于 `data`；失败时 `ok` 为 `false`、原因位于 `error`，进程退出码为 1。命令说明和版本号输出普通文本。安装器不自动修改 PATH，可使用完整路径，或自行把安装目录加入 PATH。

```powershell
& "$env:LOCALAPPDATA\Shishi\ss.exe" --help
& "$env:LOCALAPPDATA\Shishi\ss.exe" backend status
```

以下示例假定当前目录为安装目录。

## 后台与图形端

```powershell
.\ss.exe backend start
.\ss.exe backend status
.\ss.exe backend stop
.\ss.exe backend stop --force
.\ss.exe ui open
.\ss.exe ui close
```

普通查询和修改在后台未启动时自动启动，并保留 CLI 常驻标识，直到执行 `backend stop`。重复启动不增加计数；关闭终端不会释放这个标识。`backend status` 是只读探测，不会启动后台。

图形端启动会取得图形端标识。窗口叉号仍隐藏到托盘；托盘“退出”或 `ui close` 才释放图形端标识。两端都释放后，后台停止接受新操作，中断本进程拥有的任务，并等待已接受请求和数据库事务收尾。关闭本地后台不会替用户停止已经交接给外部桌面的任务。

另一端仍开启时，默认只退出当前端。图形端确认框默认选择“不强制”；交互式终端使用 `[y/N]`。重定向或其他非交互调用直接采用“不强制”，不会等待输入。`--force` 明确释放两端标识并关闭整个后台；未保存的界面草稿和未完成工作可能中断，已保存数据保留。

两个标识只存在于当前后台内存，异常退出后不恢复旧标识。重启由本次启动入口重新取得标识；正常更新或数据目录迁移会传递当前两端状态。后台按数据目录保持单实例，使用随机凭据保护仅监听回环地址的本机接口。

后台复用桌面宿主，主窗口可以不显示，仍需要 WebView2 Runtime。托盘可以随时打开图形端。

## 查询事项与记忆

```powershell
.\ss.exe tasks list
.\ss.exe tasks list --params '{"focus":"被阻塞","limit":20,"offset":0}'
.\ss.exe tasks list --params '{"status":"待处理","archived":false,"limit":20,"offset":0}'
.\ss.exe tasks get '<事项ID>'
.\ss.exe memory read
.\ss.exe methods
```

`tasks list` 支持 `query`、`status`、`focus`、`archived`、`limit` 和 `offset`，默认每页 20 条、最多 100 条，返回当前页和总数。结果按有效事项等级从高到低、同等级按更新时间倒序排列；其中延后事项按恢复时间从近到远排列，保留非延后事项的位置。使用 `tasks get` 读取完整事项，再修改需要的字段。

`focus` 与界面关注区一致，可选待处理、处理中、被阻塞或延后处理。四类互斥，排除已结束、归档和合并事项：有效延后优先，其次是阻塞或待回复，最后按底层状态分类。`status` 仍筛选底层状态；同时指定时取交集。列表结果中的 `focus` 表示当前归属，无归属时为 `null`。

`methods` 从同一个业务分发入口列出操作名，并提供常用参数示例。`call` 覆盖页面的业务操作，包括事项编辑与状态、合并、定时事项、标签、来源、插件、提示词、记忆、报表、归档、AI 接入、处置和工具管理。文件选择器 `pick` 属于界面交互；CLI 向对应业务方法直接传文件路径。`storage_change` 可由 CLI 发起，但沿用现有原生迁移进度窗口。

## 修改数据

通用形式为 `ss call <页面业务方法>`。复杂内容建议使用 JSON 文件或标准输入，避免中文、引号和多行正文在 shell 中被重新解释。

创建事项的 `task.json` 示例：

```json
{"task":{"title":"示例事项","description":"需要跟进的内容","status":"待处理"}}
```

```powershell
.\ss.exe call edit --input task.json
.\ss.exe call action --params '{"id":"<事项ID>","action":"done"}'
.\ss.exe call memory_save --input memory.json
Get-Content -Raw -Encoding utf8 task.json | .\ss.exe call edit --input -
```

Windows PowerShell 5.1 的管道编码需先设置 `$OutputEncoding = [Text.UTF8Encoding]::new($false)`；直接使用 `--input task.json` 不受管道编码影响。

修改已有事项必须传入原 `id` 和刚读到的整数 `updated`，并包含需要修改的字段；其余字段沿用原值。事项已变化时提交失败，应重新读取并合并意图。记忆保存使用 `memory_read` 返回的 `revision` 作为整数 `expected`。两类记忆均使用 `expected`、`text`，`kind` 分别为 `temporary` 或 `archived`；临时记忆最多 2000、归档记忆最多 4000 个 Unicode 字符，空字符串表示清空。读取与保存都使用完整文本，不再提交归档条目。升级保留超限旧记忆原文，新保存时遵循各自上限；版本冲突或保存失败不会覆盖当前记忆。

参数、结果和权限与页面业务一致。AI 或外部工具操作仍可能访问网络或执行较长时间；普通本地编辑不需要模型。

## 长操作

复用现有异步任务接口，提交一次后按编号查询：

```powershell
.\ss.exe call ai_job_start --params '{"method":"analyze","params":{}}'
.\ss.exe job '<返回的 job_id>'
```

返回 `done: false` 时继续查询同一编号；终态中 `ok` 和 `data` 或 `error` 表示真实结果。查询不会重新提交操作。异步编号属于当前后台进程，重启后应核对业务状态和日志，不自动重复执行。

CLI 传输失败时不重发业务请求，因为后台可能已经提交。错误会包含请求编号，便于排查。客户端超时、被关闭或断开不等于操作已经撤销。

## 隔离目录

`--root <路径>` 指定安装或隔离目录，后台读取该目录的 `data-location.json`；没有目录设置时使用其 `data` 子目录。服务未运行、服务信息残留和协议不兼容会明确返回状态或错误，不读取另一份数据库冒充结果。

离线验收使用全新的合成目录及 `--no-background-jobs --root <路径>`，禁用扫描、调度和模型后台任务，不使用实际安装数据。
