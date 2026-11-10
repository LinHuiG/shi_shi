# 拾事ShiShi

**把聊天消息整理成事项，跟进进展，再把需要调查和处置的任务交给AI。**

拾事是一个Windows桌面AI事项助手。它保留消息依据、处理记录和可编辑的记忆，让你在同一个地方查看待办、安排任务、追踪结果。

[下载Windows安装包](https://github.com/LinHuiG/shi_shi/releases/latest) · [更新记录](https://github.com/LinHuiG/shi_shi/releases) · [CLI文档](docs/CLI.md) · [插件开发](docs/PLUGIN_ADAPTATION.md)

Windows x64 · WebView2 · Rust · AGPL-3.0-only

## 能做什么

| 从哪里开始 | 拾事帮你做什么 |
| --- | --- |
| 收集消息 | 通过插件读取消息，增量导入、去重，在“消息”页查询原文。 |
| 整理事项 | 用AI分析消息，或手动创建事项；管理状态、等级、截止时间、标签和定时事项。 |
| 调查与处置 | Codex在桌面端新建对话并填好首句，由你发送后开始；DSH运行首轮后交给客户端继续。进展通过CLI写回事项。 |
| 日常跟进 | 定时分析、早报、下班汇总和归档，并接收事项变更提醒。 |
| 积累经验 | 查看、编辑AI记忆；预览提示词、规则和标签建议，再决定是否采用。 |
| 接入工具 | 配置消息插件，或让agent通过CLI查询和更新事项。 |

## 开始使用

1. **安装。** 从[Releases](https://github.com/LinHuiG/shi_shi/releases/latest)下载`Shishi-Setup-版本号.exe`。需要Windows x64和Microsoft Edge WebView2 Runtime。
2. **配置AI。** 在设置中添加AI连接，为需要使用的用途选择模型。用途模型留空时不会调用模型。
3. **创建第一条事项。** 可以先手动录入；需要接入消息时，在“消息来源”中安装可信插件并配置数据位置。
4. **跟进处理。** 查看消息依据、调整事项状态，或选择模型开始调查与处置。

安装包默认不附带消息来源插件。JSON消息来源的开发示例位于[`examples/json-source`](examples/json-source)。

### AI接入

| 接入方式 | 分析消息 | 调查与处置 |
| --- | --- | --- |
| Codex | Codex CLI | Codex桌面端，由用户发送首句 |
| DeepSeek Harness（ACP） | 支持 | 支持 |
| 兼容HTTPS API | 支持 | 不支持 |

## 数据与运行

- **数据保存在本机。** 消息、事项和配置默认位于程序目录下的`data`，可以在设置中更改目录。调用AI时，相关内容会发送给所选后端。
- **密钥单独保护。** API密钥使用Windows DPAPI保存，也可从指定环境变量读取；完整AI跟踪默认关闭。
- **插件需要信任。** 插件在独立工作进程中运行，仍具有当前用户的文件和网络权限。只安装可信插件，仅读取已授权的数据。
- **后台负责自动任务。** 关闭窗口会隐藏到托盘；托盘菜单“退出”释放图形端。CLI后台的运行与退出方式见[CLI文档](docs/CLI.md#后台与图形端)。
- **个人提示词保留。** 未修改的内置提示词随版本更新；你保存、采用或回退到的版本继续保留。归档会把适合长期保留的临时记忆并入归档记忆，提交成功后清空本批临时记忆；期间被修改或发生冲突的内容保留。

### 更新与旧版数据

开启自动更新后，拾事每天凌晨04:00安装已下载的新版本并重启，需要电脑和拾事保持运行；错过该时间不会补执行。手动覆盖安装时，可以选择强制退出当前安装目录的拾事及后台。

**1.2.6沿用1.2.5的`shishi.db`数据格式，覆盖安装保留数据目录和个人设置。** 本版不再包含旧`state.sqlite3`转换工具。从1.2.4升级时，请先通过[1.2.5的安装器](https://github.com/LinHuiG/shi_shi/releases/tag/v1.2.5)完成一次转换，并阅读该版本的迁移范围，再升级到1.2.6。

## CLI与agent

`shishi.exe`同时是图形程序和命令行入口。以下示例在安装目录中运行：

```powershell
.\shishi.exe --help | Out-String
.\shishi.exe tasks list | Out-String
.\shishi.exe backend status | Out-String
```

让agent读取安装目录中的`skills/shishi-cli/SKILL.md`，即可了解命令发现、查询、修改和异步任务的用法。仓库内对应[技能说明](docs/skills/shishi-cli/SKILL.md)；完整命令见[CLI文档](docs/CLI.md)。

<details>
<summary><strong>从源码构建</strong></summary>

需要Rust、Python和Node.js；Windows打包还需要NSIS，以及`windres`或Windows SDK的`rc.exe`。

```powershell
python scripts/test.py
python scripts/package.py
```

安装包输出到`build/Shishi-Setup-版本号.exe`。测试默认使用合成数据和模拟后端，不调用真实模型。开发约定见[开发规范](docs/DEVELOPMENT.md)。

</details>

## 许可证

原创代码采用[GNU AGPL v3.0](LICENSE)（`AGPL-3.0-only`），第三方组件保留各自许可证，见[第三方声明](THIRD_PARTY_NOTICES.md)。Release附带对应源码和`SHA256SUMS.txt`，可用于核对下载文件的完整性。

Copyright © 2026 ShiShi contributors. 软件按现状提供，不附带担保。
