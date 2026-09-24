# Nexus Launcher 1.0.2

## 中文

- 修复官方 Desktop 安装插件时包管理器可能沿源码链接修改官方运行文件的问题；安装前解除 Profile 中的官方源码投影，保留配置和外部插件链接。
- 打开设置按所选 Harness 的设置接口选择旧版 settings.yaml 或新版 Profile 配置；支持编译产物检测，未知接口明确报错。
- 快照恢复按唯一条目 id 回填当前密钥，避免配置列表重排后密钥错配；身份缺失或歧义时在写入前拒绝恢复。
- 增加 V3/V4 实时会话通知、迁移后配置恢复及插件写入边界的回归覆盖。

### 验收边界

未自动升级 Harness 或修改第三方插件。已核对 0.1.7-rc.1 关键接口，但不宣称所有第三方插件兼容或新版真实会话迁移已全面验收。程序仍保留官方源码启动与共享运行时结构。Git/SSH/LFS 和对应源码继续提供，不捆绑 GCM；离线转发请保留源码配套。运行时仍为显式路径优先，否则内置。本地 unsigned 验收不代表生产代码签名，CI 不代替所有真机安装升级验收。旧版公开附件保持不变。

## English

- Protect official runtime files from package-manager traversal of source links during Desktop plugin installation. Detach official source projections in the Profile while preserving configuration and external plugin links.
- Open legacy settings.yaml or the new Profile configuration according to the selected Harness settings interface, including compiled artifacts; report unknown layouts explicitly.
- Restore current secrets by unique entry id rather than array position, preventing credential misassignment after configuration reordering. Reject missing or ambiguous identities before writing.
- Add regression coverage for V3/V4 live session notifications, migrated configuration recovery and plugin write boundaries.

### Acceptance limits

Harness is not automatically upgraded and third-party plugins are unchanged. Key 0.1.7-rc.1 interfaces were reviewed; full third-party compatibility and real-session migration are not claimed. Official source-mode startup and shared runtimes remain. Git/SSH/LFS and corresponding source are provided without GCM; retain source companions when redistributing offline. Explicit runtime paths retain priority over bundled tools. Local unsigned acceptance is not production signing, and CI is not installation/upgrade acceptance on every device. Existing public downloads remain unchanged.
