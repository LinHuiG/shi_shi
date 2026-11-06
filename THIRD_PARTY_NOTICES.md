# 第三方组件与资源

拾事原创代码采用GNU AGPL v3.0（`AGPL-3.0-only`）；下列第三方组件仍适用各自原有许可证。

- `vendor/wry`：Wry本地补丁版本，保留其MIT、Apache-2.0许可证和上游文档。安装包随附`LICENSE-WRY-MIT`和`LICENSE-WRY-APACHE`。
- Cargo依赖：精确版本以`Cargo.lock`为准，各自适用原许可证。发布前应生成完整依赖许可清单并检查安全公告。
- `rusqlite`、`libsqlite3-sys`（MIT）：静态包含SQLite，SQLite源码以公有领域声明提供。
- `reqwest`、`native-tls`（MIT OR Apache-2.0）与`hyper`、`tokio`（MIT）：HTTP连接与异步I/O；Windows使用SChannel。`sha2`（MIT OR Apache-2.0）提供SHA-256。
- `yaml_serde`（Apache-2.0 OR MIT）、`libyaml-rs`（MIT）：读取DSH供应商配置。
- `tao`、`rfd`、`windows-sys`、`miniz_oxide`、`crc32fast`、`serde`、`serde_json`、`chrono`、`uuid`、`base64`、`thiserror`：MIT或Apache-2.0。
- WebView2 Runtime与WebView2Loader.dll：Microsoft WebView2组件；运行时由用户安装，loader随安装器分发，再分发遵守组件自身许可。
- [yetone/magpie](https://github.com/yetone/magpie)（MIT）：Codex驱动用Codex登录凭据调用ChatGPT Codex后端的做法参考该项目。该后端不是公开API，使用前请自行确认符合OpenAI服务条款。
- [Fluent UI System Icons](https://github.com/microsoft/fluentui-system-icons)（MIT，Copyright (c) 2020 Microsoft Corporation）：`assets/ui/scripts/icons.js`内联的界面图标路径。
- NSIS、Rust工具链、Node.js：构建或测试工具，不随应用分发。
- `assets/shishi.*`：项目标识资源。公开发布前仍需维护者确认原始素材和使用权。

不要从私人适配器复制第三方数据库、解码库、账号文件或未确认授权的资源进入此仓库。
