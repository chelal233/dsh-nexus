# Nexus Launcher 0.1.9

## 中文

### 修复的问题

1. **官方桌面窗口能正常打开，托盘操作与工作台一致。** 修复 Windows 上进程已经启动但窗口不显示的问题；托盘启动、停止、打开网页和终端复用工作台流程，检查结果、修复提示与状态刷新保持一致。工作台补齐 Desktop 关闭和重启，停止失败时不会继续启动第二个实例。
2. **不再把“还在启动”误报为失败。** 请求等待时间覆盖后台准备和兼容性检查；通信超时后先核对实际状态，避免重复启动或误弹插件修复。“可用，有警告”会列出相关可选插件与原因。
3. **Desktop 启动报错会回到 Nexus。** 官方客户端配置或插件加载失败时，启动器主动提示并展开诊断，同一次失败只提醒一次。即使进程已退出、结构化记录缺失或损坏，也读取官方错误报告并保留首个错误，帮助查找缺失服务的提供方，而不是把等待中的插件全部判为故障。
4. **Web 检查通过后才自动打开网页。** 修复先打开报错网页、随后才提示修复的体验；核心服务未就绪时在启动器中显示原因和诊断入口，不会自动停用插件。

### 新增和改进的机制

1. **减少 Web 和 Desktop 的重复启动准备。** 对已验证的 Harness 0.1.6-alpha.2，正常 Web 启动直接观察本次正式进程的启动信号及实际网页客户端，不再先启动隔离实例再启动正式实例。旧记录不能让新进程提前成功；手动检查、配置/版本切换检查及不支持此接口的版本仍使用隔离检查。依赖复制采用有限并行，减少重复扫描和桌面运行时重复读取，保留完整性与链接检查。同机同配置单次 Web 对比从约 67 秒降至 10.4 秒；这不是重启系统后的冷缓存测试，也不是所有机器的速度承诺。
2. **桌面端也有启动检查。** 检查实际官方客户端的加载结果，不以进程存在代替成功。检查不受支持或等待过久时显示“未验证”；失败时保留官方修复入口。检查只覆盖启动阶段，不承诺捕获运行中的全部业务错误。
3. **配置与终端操作范围更清楚。** Web 显示选中的配置档，Desktop 明确使用独立的 `desktop` 配置，二者插件设置不会自动同步。DSH 终端打开时列出绑定配置、工作目录、数据目录、Harness 来源及 Desktop 配置目录，说明 dsh 与 npm/pnpm 的目标区别；这是打开时的快照，切换配置后应重开终端。配置档展开后按插件清单、快照清单、已保存检查点排列。
4. **更新由用户确认后下载。** 发现新版本只提示可更新；点击后先打开确认窗口，确认下载后显示进度，校验完成才提供“稍后重启”与“更新并重启”。下载完成不会直接安装；应用更新前请保存工作，更新会停止 Harness 并重启 Nexus。
5. **新装插件市场默认使用 dshmarket 1.52.0。** 上游修正了官方 Desktop 配置识别及部分请求来源检查，减少误操作 Web 配置的情况。该版本仅作为首次安装默认值，不自动替换已有配置中的市场，也不代表下面列出的 Desktop 包管理问题已解决。

### 已知缺陷与兼容边界

- **官方 Desktop 内插件市场的安装、更新或重启仍可能失败。** dshmarket 1.52.0 虽识别 Desktop 配置，但包管理操作尚未完整接入官方 Desktop 提供的环境；升级市场不能保证消除 dsh/pnpm 或重启报错。Nexus 不伪造配置来源、不强行回退 Web，也不自动改写上游插件，等待上游修复。
- **第三方命令输出乱码未作为本版通用修复交付。** 排查时在单台机器验证过输出解码修正，但这不在 Nexus 安装包内，插件更新可能覆盖该本地修改。已损坏的 Harness 依赖链接也不会仅因升级 Nexus 而自动恢复；本版不宣称解决所有依赖损坏。
- **启动检查不是完整运行时监控。** “未验证”不等于确认失败，启动通过也不代表每个会话、工具、插件功能都兼容。首次准备仍受磁盘、插件数量和配置影响。
- **平台范围不扩大到上游未支持的平台。** 官方 Desktop 提供 Windows x64、macOS Intel/Apple Silicon；Windows ARM64 和 Linux ARM64 提供 Web 模式。Linux ARM64 提供 AppImage、DEB、RPM，但不等于所有麒麟、统信或其他发行版均经过真机验收。
- **离线原则不变。** Nexus 自带必需运行时；取得 Harness 后，桌面准备和同系统同架构的完整离线包导入不需要联网。第三方插件下载仍需要网络，配置/数据部分包不能替代完整离线包。

## English

### Problems fixed

1. **Official Desktop opens visibly, and tray actions match the workbench.** Fix Windows launches where the process started without a visible window. Tray start, stop, open-page and terminal actions share the workbench flow, including checks, recovery feedback and status refresh. The workbench adds Desktop Close and Restart; a failed stop never starts a second instance.
2. **Waiting is no longer mistaken for failure.** Request time budgets accommodate background preparation and compatibility checks. After a transport timeout, Nexus checks actual state before repeating a launch or suggesting plugin repair. “Ready with warnings” identifies the affected optional plugins and their reported reasons.
3. **Desktop startup errors surface in Nexus.** Configuration or plugin-loading failures bring the launcher forward and expand diagnostics, once per failed operation. Failure details survive process exit. Official error reports are read even if structured evidence is missing or malformed, preserving the first cause so users can investigate missing service providers instead of blaming every waiting plugin.
4. **Web opens automatically only after client checks pass.** Blocking failures stay in the launcher with their reason and diagnostic entry point, instead of opening a broken page before offering recovery. Nexus does not automatically disable plugins.

### New and improved mechanisms

1. **Less repeated preparation for Web and Desktop.** For verified Harness 0.1.6-alpha.2, normal Web startup observes the actual managed process and browser client instead of starting an isolated probe and then launching again. Stale evidence cannot mark a new process ready. Manual checks, profile/version-switch checks and unsupported versions retain isolated checks. Bounded parallel dependency copying and fewer repeated scans/archive reads reduce preparation while retaining integrity and link checks. One same-machine, same-profile Web comparison improved from about 67 to 10.4 seconds; this was not a post-reboot cold-cache test or a guarantee for every machine.
2. **Desktop has startup checks too.** Nexus observes the actual official client rather than treating a running process as success. Unsupported checks or long waits remain “Unverified”, and official recovery remains available on failure. These checks cover startup, not every subsequent application error.
3. **Clearer profile and terminal scope.** Web shows the selected profile; Desktop uses its independent `desktop` profile, without automatically synchronizing plugin settings. The DSH terminal opening banner lists its bound profile, working directory, data home, Harness source and Desktop profile directory, and explains dsh versus npm/pnpm targeting. It is an opening snapshot; reopen the terminal after switching profiles. Expanded profiles list plugins, snapshots, then saved checkpoints.
4. **Updates download only after confirmation.** Finding a version only announces availability. Clicking Update opens a confirmation dialog; confirming starts the download with progress. Only a verified download offers Restart later or Update and restart. Download completion never installs automatically. Save work before applying an update: it stops Harness and restarts Nexus.
5. **Fresh marketplace installations default to dshmarket 1.52.0.** Upstream fixes official Desktop profile recognition and some request-origin checks, reducing accidental targeting of Web profiles. This is a first-install default, not an automatic replacement of existing installations, and it does not resolve the Desktop package-management limitation below.

### Known issues and compatibility boundaries

- **Marketplace install, update or restart inside official Desktop can still fail.** Although dshmarket 1.52.0 recognizes the Desktop profile, its package operations do not fully use the environment provided by official Desktop. Upgrading the marketplace does not guarantee that dsh/pnpm or restart errors disappear. Nexus does not spoof profile ownership, force a Web fallback or automatically modify upstream plugins; an upstream fix is still needed.
- **Third-party command-output encoding is not a general fix shipped in this release.** A decoding adjustment was verified on one machine during investigation, but is not included in Nexus packages and can be overwritten by a plugin update. Existing damaged Harness dependency links likewise are not automatically restored just by upgrading Nexus; this release does not claim to repair every dependency failure.
- **Startup checking is not comprehensive runtime monitoring.** “Unverified” does not mean a confirmed failure, and passing startup does not validate every conversation, tool or plugin feature. First-run preparation still depends on storage, plugin count and configuration.
- **Platform scope stays within upstream support.** Official Desktop is available on Windows x64 and macOS Intel/Apple Silicon. Windows ARM64 and Linux ARM64 provide Web mode. Linux ARM64 offers AppImage, DEB and RPM, without claiming device acceptance on every Kylin, UOS or other distribution.
- **The offline contract is unchanged.** Required runtimes ship with Nexus. After acquiring Harness, Desktop preparation and full offline-package import on the same OS and architecture need no network. Third-party plugin downloads still require a connection; configuration/data-only packages cannot replace full offline packages.

## 下载与验证 / Downloads and verification

Windows x64/ARM64：EXE 或 ZIP；macOS Intel/Apple Silicon：DMG 或 ZIP；Linux ARM64：AppImage、DEB、RPM。ZIP 必须完整解压，不能只复制可执行文件；免安装不代表用户数据随目录移动。

Windows x64/ARM64: EXE or ZIP; macOS Intel/Apple Silicon: DMG or ZIP; Linux ARM64: AppImage, DEB or RPM. Extract ZIP packages completely; do not copy only the executable. Portable installation does not make user data portable.

以同一提交的五平台 Actions 结果、附件内的 `_build.json` 和 SHA256 清单确认构建来源与自动化检查范围。摘要签名不是操作系统代码签名；构建和安装包冒烟不等于所有系统、完整业务、输入法和自动更新的真机验收。本地测试中显式跳过的环境相关用例不计入通过数量。

Use the five-platform Actions results for the same commit, packaged `_build.json` files and SHA256 manifests to verify provenance and automated check scope. Manifest signing is not OS code signing. Builds and package smoke checks do not establish device acceptance for every OS, workflow, IME or automatic-update scenario. Environment-specific tests explicitly skipped locally are not counted as passed.
