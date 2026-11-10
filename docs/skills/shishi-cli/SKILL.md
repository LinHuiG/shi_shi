---
name: shishi-cli
description: 使用拾事 ShiShi 的本机 CLI 查询和维护事项、记忆及后台任务。用户要求操作拾事数据或调用其已有业务能力时使用。
---

# ShiShi CLI

通过拾事安装目录中的 `shishi.exe` 加命令操作（不带命令会打开图形界面），与图形端共用校验、锁和持久化。不要直接修改 SQLite、状态文件或后台凭据。若本文末尾附有当前实例入口，沿用其中的路径和根目录；否则优先使用用户提供的 CLI 路径，再检查 `%LOCALAPPDATA%\Shishi\shishi.exe`。不存在时通过安装信息定位，不自行安装另一份或创建新数据目录。

## 查找命令与读取数据

PowerShell 使用 `& '<安装目录>\shishi.exe' <命令>` 调用，并捕获输出（赋值给变量或接 `| Out-String`），否则 PowerShell 不等待这个图形程序结束，也拿不到 `$LASTEXITCODE`。先运行 `--help`，需要通用方法参数时运行 `methods`；其清单来自当前版本的业务接口。

```powershell
& '<安装目录>\shishi.exe' backend status
& '<安装目录>\shishi.exe' tasks list --params '{"query":"141","limit":20,"offset":0}'
& '<安装目录>\shishi.exe' tasks get '<返回的事项 id>'
& '<安装目录>\shishi.exe' memory read
& '<安装目录>\shishi.exe' methods
```

普通命令输出UTF-8 JSON，先检查`ok`，结果在`data`，失败原因在`error`。失败退出码为1；`--help`、`--version`和成功的`handling brief <处置ID>`输出文本。`handling brief`读取处置说明，并在存在`CODEX_THREAD_ID`时绑定当前Codex会话；失败仍返回JSON。事项列表按`limit`／`offset`分页，可用`focus`筛选待处理、处理中、被阻塞、延后处理；界面`#编号`可搜索，但修改时必须用返回的真实`id`。

## 修改与长操作

按用户已授权的范围操作，只读取所需资料。使用 `call <方法名>` 调用当前 `methods` 中的方法。复杂、多行或含引号的参数写入 UTF-8 JSON 文件，再用 `--input <文件>` 提交，避免 shell 改写内容。

```powershell
& '<安装目录>\shishi.exe' call edit --input task.json
& '<安装目录>\shishi.exe' call memory_save --input memory.json
& '<安装目录>\shishi.exe' call ai_job_start --params '{"method":"analyze","params":{}}'
& '<安装目录>\shishi.exe' job '<返回的 job_id>'
```

- 新建事项的 JSON 为 `{"task":{"title":"示例事项","description":"需要跟进的内容","status":"待处理"}}`。修改已有事项先 `tasks get`，在 `task` 中传原 `id`、整数 `updated` 及需要修改的字段。冲突后重新读取并合并用户意图。
- 记忆保存参数为 `{"kind":"temporary或archived","expected":读取到的整数revision,"text":"完整新正文"}`；临时记忆最多 2000、归档记忆最多 4000 个 Unicode 字符，空字符串表示清空。先读完整正文再修改，不能把摘要当作完整替换内容。
- AI 长操作提交一次后轮询同一个 `job_id`。`done:false` 表示仍在执行；终态查看内部 `ok` 和 `data`／`error`。查询状态不会重新提交操作。
- 传输失败、客户端超时或后台重启后，先按请求编号、日志及业务状态核对结果；不要自动重发可能已经执行的修改、命令或 AI 请求。异步编号只在原后台进程有效。

普通查询和修改会自动启动后台，并保持 CLI 常驻标识；`backend status` 仅探测。`backend stop` 释放 CLI 标识，图形端仍开着时后台保留。只有用户要求关闭整个后台时才使用 `--force`；关闭本地后台也不等于停止外部 AI 会话。

AI 分析、归档和外部工具可能产生模型调用或外部操作，不能把读取本技能视为授权。已有授权不重复索取。详细方法、输入格式和隔离运行方式见同安装包的 [CLI.md](../../CLI.md)；普通操作不要使用 `--root` 指向另一份数据。
