# 第三方组件与资源

拾事原创代码采用 GNU AGPL v3.0（`AGPL-3.0-only`）；下列第三方组件仍适用各自原有许可证。

- `vendor/wry`：Wry 本地补丁版本，保留其 MIT、Apache-2.0 许可证、SPDX 信息和上游文档；主项目许可证不替换这些文件。安装包随附 `LICENSE-WRY-MIT` 和 `LICENSE-WRY-APACHE`。
- Cargo 依赖：精确版本以 `Cargo.lock` 为准，各自适用原许可证。发布者应在发布前生成完整依赖许可清单并检查安全公告。
- WebView2 Runtime 与 WebView2Loader.dll：Microsoft WebView2 组件；运行时由用户安装，loader 随安装器分发。再分发时应遵守组件自身许可。
- NSIS、Rust 工具链、Node.js、Playwright：构建或测试工具；不是私人配置的一部分，不把整个本机工具目录随应用分发。
- `assets/shishi.*`：项目标识资源。公开发布前仍需维护者确认原始素材和使用权；本文件不宣称已经完成来源授权核验。

不要从私人适配器复制第三方数据库、解码库、账号文件或未确认授权的资源进入此仓库。

## YAML 供应商配置

`serde_yaml_ng` (Apache-2.0 OR MIT) and `unsafe-libyaml` (MIT) parse the existing DSH provider configuration. See the package license files distributed in the Cargo registry source archives. Provider credentials and personal configuration are not included in this repository or installer.
