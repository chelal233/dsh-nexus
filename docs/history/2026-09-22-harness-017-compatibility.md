# Harness 0.1.7 alpha compatibility / 兼容检查

核对版本：`dsh-v0.1.7-alpha.1`，上游提交 `c36a83ff6bb95e3f82cf79f9be7c724270a8aa61`。

## 修正 / Changes

- Web 冷启动不再按版本号放行。检查实际 CLI 的就绪通知、启动诊断及 Web profile 能力；能力不足保留隔离检查并记录原因。浏览器仍必须等待当前客户端真正就绪。
  Web startup uses readiness, diagnostics and profile capability checks instead of a release allowlist. Unsupported artifacts retain isolated checks with a logged reason. Browser opening still requires current-client readiness.
- 同时识别旧 Desktop 专用锁文件与新的共享运行时锁文件。按当前平台的全部依赖声明和摘要比较离线包，不因新增其他平台或发行版本变化而拒绝复用。包自身的完整性校验仍保留。
  Both runtime-lock locations are supported. Reuse compares all common inputs and the complete selected target, including hashes; unrelated target additions and carrier release labels do not invalidate identical payloads. Kit integrity validation is retained.
- 0.1.7 的运行时读取器明确兼容旧 `components` 清单；现有 Windows x64、macOS x64/arm64 离线资源与新版本所需内容一致，无需增加 Electron 或启动时下载。离线导出使用相同的依赖匹配规则。
  Upstream explicitly accepts the legacy `components` manifest. Existing Windows x64 and macOS x64/arm64 payloads match the new requirements; no additional Electron or launch-time downloads are needed. Offline export applies the same matching rules.
- 插件重复 ID 和替换内置项诊断支持 `dsh.bundle.patch` 多文件列表；检查器版本更新，旧缓存不作为新检查结果。
  Duplicate-ID and built-in replacement diagnostics support multiple bundle patch files. The checker revision invalidates previous cached results.
- Desktop 探针同时检查桌面构建目标。共享运行时包含 Linux 不代表官方桌面构建支持 Linux；本版本上游桌面构建仍仅声明 Windows x64、macOS x64/arm64。
  Desktop detection also checks the shell's declared build targets. Linux interpreter payloads do not imply Linux Desktop support; this upstream release still declares only Windows x64 and macOS x64/arm64 Desktop builds.

## 数据与验收边界 / Data and validation boundaries

上游新增 Session V4，V3 读取方拒绝更新代际。切回旧程序槽位不等于会话数据降级；应保留升级前数据备份，Nexus 不重写上游会话格式。

Upstream introduces Session V4; V3 readers reject newer generations. Switching back to an older program slot does not downgrade session data. Retain a pre-upgrade data backup; Nexus does not rewrite upstream session formats.

检查包括官方源码及锁文件对照、三个已有 Desktop 平台的资源匹配、启动观察协议核对、Rust 能力探针测试、Desktop/离线导出/兼容诊断回归。未将这些检查声称为 0.1.7 完整 GUI 真机启动或跨系统安装验收。

Checks cover upstream source and locks, payload matching for the three existing Desktop targets, startup observation contracts, Rust capability tests, and Desktop/offline-export/diagnostic regressions. These do not constitute complete 0.1.7 GUI or cross-platform installation acceptance.

Sources: [upstream tag](https://github.com/deepseek-ai/deepseek-harness/tree/dsh-v0.1.7-alpha.1), [runtime parser](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.7-alpha.1/packages/skill/tool-workspace-dependencies/src/index.ts), [Desktop targets](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.7-alpha.1/apps/desktop/scripts/desktop-build-paths.mjs), [session format change](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.7-alpha.1/docs/persistence-changes/2026-09-16-session-format-v4.zh.md).
