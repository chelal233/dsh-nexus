# Nexus Launcher 1.0.0

## 中文

### 使用体验与修复

- **Harness 未启动，也能管理和修复配置。** 插件清单、启用、禁用、安装、检查及移除优先复用所选 Harness 的官方管理器和保护规则；新插件默认保持禁用，禁用不会删除依赖。旧版本保留兼容入口。
- **正常启动先运行，失败才诊断。** 减少每次启动前重复拷贝和兼容检查；只有证据明确指向缺失依赖时，才允许有限修复并重试一次。配置、版本或停止操作发生变化时，旧恢复任务不会继续启动。
- **启动超时与真正失败分别留档。** 超时只表示尚未确认就绪，不会擅自停止或重启仍在运行的 Harness；之后若发生真实失败，仍会保存新的错误证据。
- **修复 Desktop 的旧官方包遮蔽问题。** 启动时绑定当前版本的官方模块，并备份遮蔽它们的旧官方包，避免旧 settings 提供方与新接口冲突。第三方插件和配置保持原样。
- **离线修复更可控。** 配置修复提供预览、备份、复查与恢复路径；不以禁用所有等待服务的插件代替定位。Node、pnpm 和 Git 优先使用明确指定的路径，否则使用内置工具，未采用系统环境优先的新规则。
- **版本切换更直接。** 安装准备完成后可直接切换至该版本；上游 0.1.7 的兼容判断按实际能力和构建目标处理，不用版本白名单决定启动加速。
- **配置与历史记录更清晰。** 快照、检查点按本机时区显示时间，详情使用抽屉并提供可读配置和源码视图；补齐插件版本、仓库链接、问题标记及删除相关入口。
- **设置错误不再静默忽略。** 修复随机端口 0 等配置传递问题；明确工具路径覆盖的适用范围，日志级别或系统开机启动设置失败时显示原因。

### 离线工具与分发材料

- 内置完整 Git 命令行、SSH 和 Git LFS，不捆绑可选 GCM 网页登录助手及其依赖。不会修改用户系统或全局 Git 配置；需要额外登录助手时可显式指定自行配置的 Git。
- 运行所需工具仍随 Nexus 提供。Git 及相关组件的对应源码作为独立附件与安装包一同提供，不计入日常运行时加载；再次分发二进制时应同时满足其源码交付条件。
- 发行流程校验通知原文摘要、源码清单、源码附件和安装包来源，材料不完整时不放行发布。

### 已知限制与验收范围

- Nexus 负责启动、就绪判断及离线修复，不承诺捕获所有第三方插件运行时错误。会话管理插件历史 JSON 错误在隔离官方 Desktop 后端中未复现：相关接口返回 200 和有效 JSON，不能据此宣称原故障已修复或确认上游有错。
- 官方 Desktop 仅在上游与 Electron 支持的交集内启用。Nexus 支持某个平台，不等于对应 Harness 提供该平台的 Desktop。
- 切回旧 Harness 槽位不会降级上游会话数据格式；升级前请保留数据备份。
- 自动化测试、真实子进程测试、Windows 隔离安装升级与五平台 CI 分开记录；跳过项不计通过。CI 安装包冒烟不替代所有发行版、输入法、显示器和完整业务流程的真机验收。

## English

### User-visible changes and fixes

- **Manage and repair profiles while Harness is stopped.** Plugin listing, enabling, disabling, installation, checks and removal use the selected Harness version's official manager and protection rules where available. Newly installed plugins remain disabled until enabled; disabling retains dependencies. Older versions retain a compatibility path.
- **Run first and diagnose failures.** Normal startup avoids repeating profile copies and compatibility checks. Only evidence of missing dependencies permits one bounded repair and retry. A changed profile, release or stop request cancels stale recovery work.
- **Preserve separate timeout and failure evidence.** A timeout means readiness is unverified; it does not stop or restart a running Harness. A later real failure still receives a separate diagnostic record.
- **Fix stale official packages shadowing Desktop modules.** Desktop binds official modules from the selected release and backs up older official packages that shadow them, including incompatible settings providers. Third-party plugins and profile settings are preserved.
- **Make offline repair controlled and recoverable.** Repairs provide previews, backups, rechecks and recovery paths. Waiting service consumers are not all disabled as a substitute for finding the provider failure. Node, pnpm and Git use an explicit configured path first and bundled tools otherwise; the proposed system-environment priority was cancelled.
- **Switch prepared versions directly.** A completed preparation offers a switch-to-version action. Harness 0.1.7 compatibility and startup acceleration follow capabilities and declared build targets rather than a version allowlist.
- **Clarify profiles and history.** Snapshots and checkpoints show local-time timestamps in drawer details, with readable and source views. Plugin versions, repository links, problem markers and removal entries are included.
- **Report settings failures.** Random port 0 and related preference forwarding are fixed. Tool-path override scope is documented; log-level and OS startup-setting failures surface their reasons.

### Offline tools and distribution materials

- Bundled Git includes its command line, SSH and Git LFS, but excludes the optional GCM browser-login helper and its dependencies. System and user Git configuration are not changed. Users needing another login helper can explicitly select their own configured Git.
- Runtime tools remain bundled with Nexus. Corresponding source for Git and related components accompanies installers as a separate release attachment and is not loaded during normal use. Redistributing binaries must also satisfy the applicable source-delivery conditions.
- Publication checks original-notice hashes, the source inventory, source attachments and installer provenance. Incomplete materials prevent publication.

### Known limits and acceptance scope

- Nexus handles startup, readiness and offline repair; it does not guarantee capture of every third-party runtime exception. The historical session-manager JSON error was not reproduced with an isolated official Desktop Host: the relevant APIs returned HTTP 200 and valid JSON. This does not establish that the original error is fixed or that upstream is at fault.
- Official Desktop is enabled only where both upstream and Electron support it. A Nexus package for a platform does not imply that Harness provides Desktop there.
- Switching to an older Harness slot does not downgrade upstream session data formats. Keep a pre-upgrade data backup.
- Automated tests, real subprocess tests, isolated Windows installer upgrades and five-platform CI are reported separately. Skips are not passes. CI package smoke tests do not replace device acceptance for every distribution, IME, display or complete workflow.
