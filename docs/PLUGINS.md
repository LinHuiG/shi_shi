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

打开拾事“消息来源”，选择或拖入 ZIP。宿主先检查描述、文件与路径。普通配置表单调用 `discover`；声明自定义页面的插件直接加载其页面，由页面决定何时发现账号。启用来源前由宿主调用 `validate`。插件没有独立启用开关，采集仅由接入源开关控制；没有启用的来源时不启动采集。

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
| `settings` | 可选自定义配置页，见下文；未声明时继续使用普通表单 |

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

## 页面内配置（1.2.2 起）

在 manifest 中增加以下声明，同时把 `settings.html` 列入 `files`。页面使用内联脚本、样式和 data 图片，不加载网络或外部文件。`config_fields` 可省略；已有声明继续用于完成时的字段检查。

```json
"settings": {
  "api_version": 1,
  "entrypoint": "settings.html",
  "methods": ["ui_load", "ui_validate"]
}
```

宿主把 HTML 放在来源编辑页的沙箱 iframe 内，来源名称与启用状态由宿主管理。1.2.2 起，来源开关放在标题右侧，名称自动生成，不再单独显示名称输入框。打开时直接传入当前配置，新来源可能是空对象；不要依赖宿主先调用 `discover`。页面必须处理自身加载、发现及错误状态。

```js
const view = await window.shishiPlugin.call('ui_load');
const validated = await window.shishiPlugin.call('ui_validate', draft);
await window.shishiPlugin.complete(validated.config);
// 或离开当前配置页：await window.shishiPlugin.cancel();
```

`call` 只能调用声明的方法。`complete` 把插件配置与当前来源名称、启用状态一起交给宿主；宿主检查字段、账号一致性并持久保存，成功才 resolve 和关闭页面。保存失败时 reject，页面及输入保留，可直接重试。不要在调用 `complete` 前自行关闭页面。保存中不能取消；其他在途配置调用仍可取消。

`cancel` 直接回到来源列表，放弃宿主尚未保存的编辑。插件在先前 `call` 中明确保存到自己文件的数据不会自动回滚，应在按钮和提示中说明保存对象。

1.2.2 起，插件可用 `await shishiPlugin.suggestSourceName?.(name)` 建议来源名称。名称由插件根据账号生成，须为 1–100 字；返回 `true` 表示已填入草稿，`false` 表示保留现有名称。宿主仅接受空白或插件默认名称，同一编辑期间可更新先前建议；既有自定义名不覆盖。此调用不保存数据，仍由 `complete` 统一提交。可选调用方式兼容旧宿主。

页面还可用 `await shishiPlugin.setDirty?.(boolean)` 报告草稿是否与打开时的持久配置不同。宿主同时检查启用状态，只在未修改时允许点击配置页外关闭；搜索、筛选和滚动不应标记修改。保存中不接受状态更新。插件保存成功但宿主 `complete` 失败时应保持修改标记，供重试；旧插件未提供状态时，宿主根据输入事件保守地保留页面。取消仍不保存宿主草稿。

1.2.2 起，页面可选择以下宿主能力：

```js
const path = await shishiPlugin.pickPath?.({
  kind: 'file', // 或 directory
  title: '程序',
  defaultPath: draft.program || '',
  filters: [{ name: '程序', extensions: ['exe'] }],
});
if (path != null) draft.program = path;
// 自然内容高度包含内边距和底部操作；null 恢复填满可用区域。
await shishiPlugin.setContentHeight?.(240);
```

路径选择返回字符串或 `null`；取消不应清空输入。宿主仅接受当前 iframe 会话的请求，选择期间保留页面，返回路径不会自动保存。插件应同时允许粘贴路径。

高度上报应根据实际内容计算，并对重复值去重；内容或宽度变化后重新计算，不得依赖 iframe 当前视口高度，避免循环缩放。宿主限制最大高度，较短内容自动收紧，长列表由插件内部滚动。

配置页优先自动发现和校验，只在失败时展开缺失路径；已有有效值、手工输入、勾选和错误重试均应保留。多账号需要明确选择，不能自动换掉已有绑定。普通界面保留保存／取消，技术字段收进高级设置。

配置页使用独立的可取消 ABI，不改变普通消息接口：

```c
typedef uint32_t (*shishi_cancelled)(uintptr_t context);
uint8_t *shishi_plugin_settings_call(
    const uint8_t *input, size_t input_len, size_t *output_len,
    shishi_cancelled cancelled, uintptr_t context);
```

输入仍是协议 1 JSON，`config` 在会话开始时固定；`params` 来自页面，`context` 提供 `plugin_dir` 和 90000 毫秒超时。输出沿用信封与 `shishi_plugin_free`。插件应定期检查取消回调，不能把回调或 context 留到函数返回后使用。

宿主在无窗口辅助进程中调用此 ABI；95 秒未退出或用户取消时终止该进程。请求最大 2 MiB，响应最大 32 MiB；HTML 最大 2 MiB，方法最多 32 个。正常退出和取消后清理临时请求与响应；异常关闭仍可能留下文件。进程隔离控制生命周期，不限制 DLL 的本机权限，也不代表来源采集接口已有崩溃隔离。

宿主不向插件导出 SQLite ABI 或共享 `sqlite3.dll`。插件依赖自己需要的数据库引擎及来源客户端格式；宿主的业务数据库不是来源数据库解密接口。

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
