# 插件接入教程

## 为什么使用插件

主程序只依赖统一消息模型，不内置某个通讯软件的数据库结构、安装位置、解码算法或账号规则。插件负责读取来源，宿主负责去重、事项分析和界面。私人适配器可以独立维护，不需要放进此仓库。

协议使用 UTF-8 JSON 和 C ABI，避免跨 DLL 传递 Rust 内存布局。插件与宿主都必须是 Windows x64；Rust 插件可使用本仓库 SDK，其他语言只要满足 ABI 和内存释放约定也可接入。

**信任边界：**插件直接在主进程执行。文件白名单、路径检查和能力声明不是权限沙箱。宿主不能强制阻止恶意 DLL 读取文件、联网或崩溃；只安装可信插件。

## 从可编译示例开始

查看 [JSON 来源示例](../examples/json-source/README.md)。在仓库根目录执行：

```powershell
cargo build -p shishi-json-source --release --locked
```

实际 Cargo 包名以示例的 `Cargo.toml` 为准。将生成的 DLL 和 `examples/json-source/plugin.json` 放在同一空目录，再把该目录内容压成 ZIP。不要把整个源码目录或 `target` 压进去。

```text
plugin.json
shishi_json_source.dll
```

打开拾事“消息来源”，选择或拖入 ZIP。宿主先检查描述、文件与路径；打开配置页面会调用 `discover`，保存时调用 `validate`。确认来源后再启用扫描。

## Manifest

完整示例见 [plugin.json](../examples/json-source/plugin.json)。关键字段：

| 字段 | 说明 |
| --- | --- |
| `id` | 稳定且唯一；字母、数字、短横线、下划线，1–64 字符 |
| `name`、`version` | 展示名称和插件版本 |
| `api_version` | 当前为 1 |
| `entrypoint` | 相对路径的 DLL，必须同时列在 `files` 中 |
| `files` | 安装资源白名单，包含 DLL 依赖；不复制未声明资源 |
| `capabilities` | 实现的接口名称 |
| `config_fields` | 声明表单字段，支持 file、directory、text、password |

禁止绝对路径、上级目录、链接和重解析点。ZIP 可以在根目录或一层目录内包含一个 manifest；不能包含多个插件描述。文件数量、包体积和请求体积均有限制，详见 [协议参考](../PLUGIN_API.txt)。

## 请求与返回

宿主请求包含 `protocol`、`method`、`config`、`params` 和 `context`。`context` 提供插件目录、工作目录及超时建议。普通配置中只保存非敏感选项或凭据引用。

| 方法 | 职责与重要约束 |
| --- | --- |
| `discover` | 可在配置不完整时调用；只查指定位置或明确关联来源，返回候选，不默认选错账号 |
| `validate` | 验证可读性、格式和账号；提供稳定 `account_key` |
| `fetch_incremental` | 根据游标和时间窗返回增量；重试幂等，不提前推进宿主游标 |
| `fetch_range` | 补读指定时间范围，不改变增量游标 |
| `lookup_messages` | 查询已知 ID 的当前状态，用于撤回及更新 |

SDK 接收 `fn(Value) -> Result<Value, String>`，宏导出 `shishi_plugin_abi`、`shishi_plugin_call`、`shishi_plugin_free`。分配内存由同一 DLL 释放。不要跨边界传递 String、Vec、trait 对象或依赖不同编译器布局的结构。

```rust
use shishi_plugin_sdk::serde_json::{json, Value};
shishi_plugin_sdk::export_plugin!(handle);

fn handle(request: Value) -> Result<Value, String> {
    if request["protocol"] != 1 { return Err("Unsupported protocol".into()); }
    match request["method"].as_str() {
        Some("discover") => Ok(json!({"candidates": [], "warnings": []})),
        // Implement validate and the three read methods before distributing.
        _ => Err("Method not implemented".into())
    }
}
```

该片段只展示入口；它不是完整可安装插件。完整实现以 JSON 示例为准。

## 消息模型

提供稳定 `id`、毫秒时间戳 `time`、`text`，并尽量提供会话、发送者及 `mine`、`private`、`mention_me`、`mention_all`、`withdrawn` 等事实字段。不确定的字段留空或 false，不推断用户身份。

同名会话不能共享错误的会话 ID；账号切换必须改变 `account_key`，否则可能错误去重。保留原始消息 ID，回复关联指向真实来源 ID。附件只提供来源范围内的本地索引，不预先发送到模型。

## 安全与运行约定

- 仅读取已授权来源，不发送消息、不修改来源数据库。
- 静态初始化不得读取数据、启动线程或要求完整配置。
- 同一 DLL 可能并发被多个来源调用，处理器必须线程安全且可重入。
- 超时为约定，不是宿主强制终止能力；需要中断或崩溃隔离时设计独立进程。
- 不留下后台线程、回调或异步工作，否则 DLL 卸载后不安全。
- 不在日志、错误、候选标签或普通配置中返回凭据。`password` 仅遮挡界面。
- 对大文件、损坏输入、撤回消息、权限不足、文件移动和重复读取作可预期处理。

## 接入验收

用合成消息测试发现、保存、增量、补读、撤回、账号切换、重复导入和并发读取；检查时间边界与游标重试。验证包中无私人配置、数据库、凭据或未声明依赖。记录 DLL 哈希、版本、来源和许可证，再交给用户安装。
