# 把你的聊天工具接入拾事

复制本说明给你的 AI，填写“聊天工具名称”和“已经授权读取的数据位置”。请 AI 先确认可读取的数据格式；不清楚时向你提问，不猜账号、不绕过登录或加密。

## 给 AI 的开发任务

为指定聊天工具开发一个拾事 Windows x64 消息来源插件。只读取我明确授权的本地数据或官方导出文件，不发消息、不修改来源数据。先用合成消息完成实现和测试，再由我选择真实数据位置。

交付可编译的独立项目、plugin.json、DLL、安装 ZIP 和简明使用说明。Rust 可使用拾事的 plugin-sdk；其他语言须严格遵循下面的 C ABI 和内存释放约定。仓库：https://github.com/LinHuiG/shi_shi ，SDK 在 plugin-sdk/，完整通用示例在 examples/json-source/。

必须完整实现 discover、validate、fetch_incremental、fetch_range、lookup_messages。采用稳定的账号、消息和会话编号，正确处理新消息、编辑、撤回、回复、重复读取、账号切换、Unicode 路径、损坏数据与权限不足。增量重试不得跳过消息；按宿主成功提交后的游标继续。未知字段留空，不编造用户身份或消息事实。

插件在宿主进程中运行，必须线程安全，不遗留后台线程；配置中不保存或输出明文凭据。图片为可选能力，仅提供授权账号目录中已缓存的本地图片。

## 独立 Rust 项目的最小清单

```toml
[package]
name = "my-chat-source"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
shishi-plugin-sdk = { path = "../shishi/plugin-sdk" }
```

SDK 路径需按实际检出位置调整。manifest 的 entrypoint 与 files 要使用这个项目实际生成的 DLL 文件名；下面附带的 manifest 对应原 JSON 示例项目，复制后必须一起调整。

## 构建与安装

Rust 独立项目的 Cargo.toml 需设置 `[lib] crate-type = ["cdylib"]`，依赖匹配版本的 shishi-plugin-sdk（可用本地 SDK 路径）。使用 Windows x64 工具链执行 `cargo build --release --locked`。下面附完整 JSON 来源实现及 manifest；更换软件时替换读取逻辑和插件唯一 id、名称及 DLL 文件名，不能仅改 manifest 就声称已适配。

ZIP 根目录放 plugin.json、DLL 和 manifest.files 明确声明的依赖，不能夹带源码、真实聊天、数据文件、密钥或 target 目录。在拾事“消息来源”安装 ZIP → 选择数据位置 → 验证 → 启用扫描。

## 交付前验收

用合成数据验证：发现来源、保存配置、新增、编辑、撤回、增量边界、补读、重复读取、游标重试、多账号隔离、并发调用、损坏输入和错误提示。给出构建命令、测试结果、包内文件清单和 DLL 哈希。首次安装默认不读取真实来源，等我配置和启用。

以下依次附带完整宿主协议、示例 manifest 和完整 JSON 来源实现。实现与验收应以这些实际约束为准。
