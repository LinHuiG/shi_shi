# 真实 AI 接入回归

此规范适用于以后所有接入、提示词协议及 AI 工作流变更。日常 Rust/UI 回归仍使用模拟接入，不消费模型额度。真实接入回归需要用户明确授权，再显式传入 `--allow-live-ai`；没有配置或测试失败必须如实记录，不能用模拟通过替代。

固定矩阵：Codex、DSH、API × 消息过滤（message_filter）、消息分析（classify）、处置与追问（handling）、报表（report）、归档（archive）。同时覆盖自然语言生成、编辑、定时事项、优化建议及提示词审校等结构化输出入口。API 处置是能力拒绝测试，不发送模型请求。

运行器只反序列化现有 state 的 config，不读取其中的事项、消息或个人提示词用于请求。它复用已配置的程序、认证和模型目录，使用独立输出目录、默认配置、最小合成提示词及合成预期结果。非 Codex 优先使用配置中的 DS 4.1 Flash 1017，其次 DS 4.1 Flash，再其次可用的 DeepSeek Flash；Codex 使用配置中的 Codex 模型。没有符合条件的已配置模型则失败，不静默改用昂贵模型。

```powershell
./tests/live-ai-regression.ps1 -ConfigRoot <运行中的SS目录> -AllowLiveAi
```

该脚本运行本仓库已构建的 release 主程序，不覆盖正在使用的安装。可用 `-OnlySource codex|dsh|api`、`-OnlyScenario <kind>` 重跑失败项；不自动重试结果不确定的处置。默认遍历全部矩阵。

用户只授权小规模 Codex 工具诊断时，使用 `-OnlySource codex -OnlyScenario tool_call`：仅发送一份最小合成请求，声明一个字符串回显工具，通过 Codex 文本响应、SS 共用 API 工具循环和命令执行器完成一次调用，并核对已保存的参数、退出状态、标准输出和最终结构化结果。沿用已配置 Codex 模型及其最低可用推理强度，总时限 120 秒；失败不自动重试。此诊断不加入默认矩阵，不使用真实消息、个人提示词或注册工具，不修改运行中的安装。

无需真实模型的 CLI 验证：`python tests/codex-tool-native.py --binary <release-shishi.exe> --codex <codex.exe> --model <本机已缓存的CLI模型编号> --output <隔离目录>`。模型标识只选取本机协议元数据，响应由 loopback 合成服务生成，不连接模型供应商、不使用账户认证。脚本在独立 home 中把 Codex 当普通模型使用：返回文本调用请求，由真实 SS 执行合成回显命令，再回传结果并取得最终响应；同时检查模型侧没有注册业务工具或 ss_tool 的 MCP 函数。脚本内部仅为调用诊断入口传入 opt-in 标记，实际模型路由被固定到本地合成服务；报告中的用量也是合成值。

`python tests/tool-bridge-native.py --binary <release-shishi.exe> --output <隔离目录>` 检查处置 MCP 的唯一 `ss_tool`、现有事项凭证、目录和 SS 父进程回调；不执行业务命令或调用模型。真实 SS 主进程中的命令执行、重名工具、开关与记录关联由合成 Rust 用例覆盖。

针对当前分析 API 的小规模诊断，可加 `-OnlySource api -OnlyScenario classify -UseAnalysisRoute`，精确使用“事项更新”已保存的接入、模型与强度，不采用首个 API 或另选模型。只发送一份合成请求；报告另含首个公开内容时间、公开内容事件数、字符数及实时输出计数样本，原始 SSE 仍只留在隔离目录。授权范围仅含单个 API 时，不扩大为全部矩阵。

结果在新的隔离目录内 `report.json`，包含来源、场景、状态、耗时和返回用量。结构化输出按实际场景 schema 校验并检查合成预期，允许不影响业务结果的 reason 说明自然变化；处置验证同一会话的首轮和追问。这是轻量协议／结构化输出矩阵，业务判断、状态事务和来源切换仍由合成 Rust/UI 回归覆盖，不宣称验证了真实业务语义。用量按接入实际报告，不把缺失当成零。临时凭据由运行器加密保存并在结束时删除；不提交配置、凭据、会话标识或本地报告。测试不得安装插件、修改用户 profile 或读取真实业务目录。桌面端可见性和原生 UI 需另做实际版本验证。

插件回归分两层：`node tests/dsh-workspace-bridge.cjs` 验证会话 ID 校验、幂等关联和 stdout 隔离；`python tests/dsh-workspace-native.py --launcher <dsh.cmd> --output <新的绝对隔离目录>` 在真实 DSH 上使用独立 home、原生持久化和工作区服务验证关联，不发送提示词，模型调用为零。它不会安装到用户 desktop profile。
