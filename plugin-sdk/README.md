# ShiShi Plugin SDK

Rust 辅助库，用于导出协议版本 1 的 C ABI。它不是进程沙箱，也不自动提供账号、存储、网络或权限能力。

处理器签名为 `fn(serde_json::Value) -> Result<serde_json::Value, String>`，通过 `shishi_plugin_sdk::export_plugin!(handle)` 导出 ABI 版本、请求入口和内存释放函数。输入输出上限为 32 MiB；必须线程安全，不得跨 DLL 传递 Rust 容器。

在独立插件的 Cargo.toml 中将本库声明为 path 或固定 Git revision 依赖，设置 `[lib] crate-type = ["cdylib"]`。只有实现全部必需方法、准备 manifest 和依赖文件后才形成完整插件包。

参见 [插件教程](../docs/PLUGINS.md)、[协议参考](../PLUGIN_API.txt) 和 [示例](../examples/json-source/README.md)。本库采用主项目 [GNU AGPL v3.0](../LICENSE)（`AGPL-3.0-only`，仅 3.0 版）。
