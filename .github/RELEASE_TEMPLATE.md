# Nexus Launcher 1.0.1

## 中文

- 采用透明“鲸鱼站长”作为统一图标，覆盖窗口、托盘、安装包与侧栏。
- 维护页诊断包移至本地依赖和离线配置档修复附近；诊断包与离线修复支持折叠，折叠不清空当前表单。
- 移除维护页重复嵌入的插件管理，保留独立“内置插件”和“配置与插件”入口。
- 修复插件仓库链接在 Electron 中点击无反应：通过受限的桌面接口打开 HTTPS GitHub 链接，拒绝其他协议、域名及带凭据地址。
- 设置页“原生集成”移至“安装包身份”上方，“启动配置说明”移至“帮助”下方。
- Windows/macOS ZIP 附件增加 `_portable.zip` 后缀，安装包名称不变；同步更新产物收集、摘要和来源校验规则。此命名不表示用户数据改存程序目录。

### 已知限制

本次未改变 Harness 数据存储或运行时优先级（显式路径优先，否则内置）。Git/SSH/LFS 与对应源码仍随发行提供，不捆绑 GCM。历史会话管理 JSON 错误仍属于未复现问题，未宣称已修复。自动测试及 CI 安装包检查不代替所有系统真机安装升级验收；本地 unsigned 验收不代表生产代码签名。原 1.0.0 下载附件保持不变。

## English

- Adopt the transparent Whale Station Master icon across the window, tray, packages and sidebar.
- Move diagnostic bundles beside local dependencies and offline profile repair. Bundles and offline repair are collapsible without clearing form state.
- Remove duplicate plugin management embedded in Maintenance, while preserving Built-in plugins and Configuration and plugins.
- Fix plugin repository links that did not open in Electron. A restricted desktop bridge opens HTTPS GitHub links and rejects other schemes, domains and credential-bearing URLs.
- Move Native integration above Release identity and Launch configuration explained below Help.
- Add `_portable.zip` to Windows/macOS ZIP attachment names, preserving installer names. Update collection, checksum and provenance checks accordingly. The filename does not imply storing user data beside the executable.

### Known limits

Harness data storage and runtime priority (explicit paths, otherwise bundled) are unchanged. Git/SSH/LFS and corresponding source remain provided, without GCM. The historical session-manager JSON error remains unreproduced, not claimed fixed. Automated and CI package checks do not replace installation/upgrade acceptance on every device; local unsigned acceptance is not production code signing. Existing 1.0.0 downloads remain unchanged.
