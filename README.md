# 拾事 · ShiShi

拾事是一款 Windows 桌面 AI 事项助手：把聊天消息整理成可跟进的事项，保留消息依据与处理经验，再由你选择事项，交给 AI 调查和处置，持续查看进展与结论。

**版本：1.2.1 · Windows x64**

1.2.1 更新 Codex 桌面处置、数据目录迁移与定时升级，增加固定事项编号、变更记录和系统日志搜索，并改进流式进度、设置与状态保存。详见 [更新说明](docs/RELEASE_1_2_1.md)。

## 主要功能

- **收集消息**：通过来源插件读取消息，增量导入、去重，在收件箱中查询原始依据。
- **管理事项**：AI 整理或手动创建，维护状态、等级、截止时间、标签和定时事项。
- **分析与回顾**：按需或定时分析，生成早报、下班汇总，归档已完成事项，维护可查看和编辑的 AI 记忆。
- **一键处置**：为事项启动持续的 AI 会话，支持追问、权限审批、中断和进展同步。
- **积累经验**：根据纠正与处理结果生成提示词、规则或标签建议，由你预览、修改和采用；相关配置保留版本供回滚。

## 设计思路

```text
来源插件 → 本地收件箱 → AI 分析 → 事项 → 用户发起 AI 处置 → 结果与归档
```

- **采集、分析、处置分开**：插件负责读消息，AI 分析负责整理事项；需要调查或执行时，由用户发起处置。消息内容本身不代表操作授权。
- **本地保存，按需提供上下文**：消息、事项和配置保存在本机；调用 AI 时，将相关内容发送给你选择的后端。补查消息和图片使用受事项范围限制的只读 MCP 接口。
- **可替换的接入层**：消息来源通过插件扩展，AI 后端独立选择，主程序不绑定某个聊天工具或行业。
- **轻量桌面结构**：Rust 负责数据、进程和协议，WebView2 展示本地界面，无需部署独立 Web 服务。

## 快速开始

1. 准备 Windows x64 和 Microsoft Edge WebView2 Runtime。从 [Releases](https://github.com/LinHuiG/shi_shi/releases) 获取安装包，或按[开发与构建说明](docs/DEVELOPMENT.md)自行构建。
2. 在设置中配置 AI 连接和可用模型：

   | 接入方式 | 事项分析、报表与归档 | 一键处置 |
   | --- | --- | --- |
   | 已登录的 Codex CLI 和桌面端 | 支持 | 支持 |
   | DeepSeek Harness（DSH，ACP） | 支持 | 支持 |
   | OpenAI Chat Completions 兼容 HTTPS API | 支持 | — |

3. 可以先手动创建事项；需要读取消息时，在“消息来源”安装可信插件，选择自己的数据位置并验证。
4. 启用扫描与分析，检查生成的事项；选中事项后可发起“一键处置”。

首次启动默认开启扫描、消息过滤和自动分析，AI 接入与用途模型留空；配置完成前不调用模型，完整 AI 跟踪默认关闭。自动任务需要程序保持运行；关闭窗口会进入托盘，使用托盘菜单“退出”结束程序。API 密钥使用 Windows DPAPI 保存，或从指定环境变量读取。

## 插件怎么接入

消息来源插件是 **Windows x64 DLL + `plugin.json`**，通过 C ABI 交换 UTF-8 JSON。Rust 可直接使用 [plugin-sdk](plugin-sdk/)；其他语言按[协议参考](PLUGIN_API.txt)实现相同接口。

1. 参考完整的 [JSON 来源示例](examples/json-source/README.md)，实现 `discover`、`validate`、`fetch_incremental`、`fetch_range`、`lookup_messages`，分别处理发现、验证、增量读取、区间补读和按 ID 查询。消息必须有稳定 ID 和毫秒时间戳，正确处理编辑、撤回、重复读取与游标重试。
2. 在 `plugin.json` 声明唯一 `id`、插件版本、`api_version: 1`、DLL 入口、文件白名单、能力和配置字段。Rust 插件使用 `cdylib`，由 SDK 的 `export_plugin!` 导出入口。
3. 在仓库根目录构建示例：

   ```powershell
   cargo build -p shishi-json-source --release --locked
   ```

4. 将 `target/release/shishi_json_source.dll` 与示例的 `plugin.json` 放入同一空目录，把目录内容压成 ZIP：

   ```text
   plugin.json
   shishi_json_source.dll
   ```

5. 在拾事“消息来源”安装 ZIP → 配置数据位置 → 验证 → 启用扫描。先用合成消息测试，再读取用户授权的数据。

插件约定只读来源，不发送消息、不修改来源数据。DLL 运行在主程序进程内，具有当前用户的权限，请只安装可信插件；发布包中不要放真实消息、账号配置或凭据。主程序不附带私人聊天工具适配器。

需要适配自己的聊天工具，可将[插件开发任务说明](docs/PLUGIN_ADAPTATION.md)交给开发者或 AI；详细约定见[插件接入教程](docs/PLUGINS.md)。

## 进一步了解

- [架构与数据流](docs/ARCHITECTURE.md) · [AI 工作流](docs/AI_WORKFLOW.md) · [AI 后端接入](AI_BACKENDS.md)
- [提示词与配置](docs/CONFIGURATION.md) · [开发、测试与构建](docs/DEVELOPMENT.md)
- [贡献说明](CONTRIBUTING.md) · [发布流程](docs/RELEASING.md) · [第三方组件](THIRD_PARTY_NOTICES.md)

本项目原创代码（主程序、AI 后端、插件 SDK 与示例）采用 [GNU AGPL v3.0](LICENSE)（`AGPL-3.0-only`，仅 3.0 版）。允许商用、修改与再分发；分发受许可覆盖的程序，或通过网络提供修改版时，须按许可证要求向相应接收者或用户提供对应源码。第三方组件保留各自许可证，见[第三方声明](THIRD_PARTY_NOTICES.md)。

Copyright (c) 2026 ShiShi contributors. 软件按现状提供，不附带担保。

Codex 分析继续使用 CLI；事项处置由桌面会话运行，需要桌面消息通道。自动更新默认开启：09:00 提醒已校验的新包，04:00 独立安装并重启；错过时点不补执行，可关闭开关，手动操作仍保留。详见 [版本更新](docs/UPDATES.md)。
