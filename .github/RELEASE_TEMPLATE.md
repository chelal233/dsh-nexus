# Nexus Launcher 0.1.10

## 中文

### 修复的问题

1. **检查没通过，就不会再打开报错网页。** 以前 Harness 进程运行且已有 URL 时，工作台和托盘可能允许打开浏览器，即使界面同时显示“客户端未就绪”。现在工作台、托盘、通知跳转及自动打开统一要求本次客户端检查通过；执行打开动作前还会重新读取状态，防止旧按钮状态误放行。检查中、未验证、核心服务受阻时保留停止、诊断和修复入口，不把进程存在当作可用。
2. **从托盘启动也能知道结果。** Windows 使用原生托盘气泡提示正在启动、已就绪、失败或未验证，点击可回到工作台处理。通知按本次启动去重，旧进程的成功记录不能让新启动提前报喜；停止与取消也会结束对应提醒。托盘气泡已由用户在 Windows 真机验收。
3. **停止、取消和重启更一致。** Web 托盘重启复用工作台流程；启动检查中可取消，取消绑定当前启动任务，旧菜单不会取消新的任务。Desktop 工作台和托盘共用重启逻辑，停止失败不启动第二个实例；重启等待停止期间再点停止，会取消后续启动。
4. **Desktop 报错也有明确处理方向。** Web 和 Desktop 共用启动错误分类，区分缺失依赖、配置损坏、接口不兼容和服务等待，保留官方错误并提供依赖检查、配置管理和日志入口。明确提示 Desktop 使用独立的 `desktop` 配置，不会因为某插件在等待服务就认定它有故障。

5. **Windows 状态文件短暂占用不再立即判为启动失败。** Desktop 状态写入遇到短暂共享冲突时，最多约 0.5 秒有限重试，保留原子替换和旧状态，永久错误仍报告。

### 新增和改进的机制

1. **离线依赖修复：先预览，再确认。** 维护页根据所选 Harness 的锁文件与本地包身份检查缺失链接，支持 pnpm 11 的缩短目录名。只补能验证名称和版本的缺失链接，不下载、不覆盖已有文件或损坏链接；执行前保存锁文件、相关清单和修复计划，执行后逐项记录并复检。过期预览必须重新检查，修复完成仍需启动对应模式验证。
2. **减少重复缓存等待，同时保留检查。** 桌面运行时缓存检查改为最多 16 个文件系统请求并行，仍遍历全部条目、验证链接范围，损坏时仍重建。同机 8,909 项缓存的只读对比中，这一阶段从约 1.5–1.8 秒降至 0.5–0.7 秒，结果与原记录一致；不代表完整冷启动耗时或所有机器的提速幅度。
3. **能看见启动时间花在哪里。** Web 展示输入检查、兼容检查和创建进程阶段耗时；Desktop 展示准备阶段明细。结束后计时冻结，新启动重新计时，不把后续使用时间算作启动耗时。
4. **更新前可以先看具体变化。** 更新确认窗口在版本号旁增加“查看更新内容”，打开对应版本的 GitHub 发布页。确认下载、进度、下载校验、稍后重启及确认安装的流程保留；隔离 Windows 更新测试覆盖中断后恢复及重启后的版本、文件摘要检查。

### 已知缺陷与兼容边界

- **上游 Desktop 插件市场的包管理问题仍未代修。** dshmarket 在官方 Desktop 内安装、更新或重启仍可能失败；本版没有伪造 PATH、服务或配置来源，也没有强制回退 Web，仍需上游修复。
- **依赖修复有明确范围。** 只处理 Nexus 管理的 Harness 版本中可验证的本地缺失链接，外部源码、已有损坏链接、缺少包文件等情况可能需要其他修复。它不是一键重装所有依赖，也不证明所有插件启动成功。
- **启动检查不等于全面运行时监控。** 未验证不等于已确认失败；检查通过不覆盖全部对话、工具和第三方插件功能。首次准备仍受磁盘、配置和插件数量影响。
- **系统通知可能被操作系统隐藏。** Windows 通知设置、勿扰模式等可能影响气泡展示，工作台和托盘状态仍保留检查结果。Windows 的用户验收不替代 macOS/Linux 通知的真机验收。
- **平台与离线原则不变。** 官方 Desktop 仍面向 Windows x64、macOS Intel/Apple Silicon；Windows ARM64 和 Linux ARM64 提供 Web 模式。Linux ARM64 提供 AppImage、DEB、RPM，不保证每种信创发行版均完成真机验收。Nexus 自带运行时；下载 Harness 后及导入同系统同架构完整离线包后的准备不依赖网络，第三方插件下载仍需要联网。

## English

### Problems fixed

1. **A pending or failed check no longer opens a broken page.** Previously a running Harness process and URL could enable the workbench or tray browser action even while the client was unavailable. Workbench, tray, notification links and automatic opening now require a successful client check for the current run. Opening re-reads the latest status to reject stale enabled buttons. Stop, diagnosis and repair remain available while checking, unverified or blocked; a process alone is not readiness.
2. **Tray launches report their outcome.** Native Windows balloons show starting, ready, failed or unverified states and open the workbench when clicked. Results are deduplicated per launch, and success from an old run cannot announce a new launch as ready. Stop and cancellation end the corresponding feedback. A user has accepted the balloons on a real Windows machine.
3. **More consistent stop, cancel and restart.** Web tray restart uses the workbench flow. Startup checks can be cancelled, with cancellation bound to the displayed operation so stale menus cannot cancel new work. Desktop workbench and tray share restart behavior: a failed stop never starts another instance, and another Stop during restart cancels its pending launch.
4. **Actionable Desktop startup errors.** Web and Desktop share classification for missing dependencies, damaged configuration, incompatible interfaces and waiting services. Official details remain available with dependency inspection, profile management and log entry points. Guidance names the independent `desktop` profile and does not blame a plugin merely because it is waiting for a service.

5. **Brief Windows state-file locks no longer immediately fail startup.** Desktop state writes retry transient sharing conflicts for at most about 0.5 seconds, retaining atomic replacement and the prior state. Persistent errors are still reported.

### New and improved mechanisms

1. **Preview and confirm offline dependency repairs.** Maintenance checks missing links against the selected Harness lockfile and local package identity, including pnpm 11 shortened directory names. It only restores missing links to verified packages, without downloading or replacing existing files or broken links. Before changes it records the lockfile, relevant manifests and plan; afterwards it journals and rechecks results. Stale previews require a new inspection. The affected mode still needs a startup check after repair.
2. **Less cache waiting without skipping verification.** Desktop runtime cache checks use at most 16 concurrent filesystem requests while inspecting every entry and link boundary and rebuilding damaged caches. A read-only comparison of 8,909 entries on the same machine reduced this stage from about 1.5–1.8 seconds to 0.5–0.7 seconds, with identical inventory results. This is not a total cold-start measurement or a guarantee for every machine.
3. **See where startup time is spent.** Web shows input checks, compatibility checks and process creation; Desktop shows preparation stages. Timings freeze on completion and reset for a new launch, excluding subsequent usage time.
4. **Read changes before downloading an update.** The update confirmation dialog adds View release notes beside the version, opening that version's GitHub page. Download consent, progress, validation, deferred restart and explicit installation remain in place. An isolated Windows update test covered interrupted-download recovery and post-restart version and file-hash checks.

### Known issues and compatibility boundaries

- **Upstream Desktop marketplace package management remains unresolved.** Installing, updating or restarting through dshmarket inside official Desktop can still fail. This release does not spoof PATH, services or profile ownership, nor force a Web fallback; an upstream fix is still needed.
- **Dependency repair has a defined scope.** It restores verified missing links within Nexus-managed Harness versions. External sources, existing broken links and missing package files can require other recovery. It does not reinstall every dependency or prove every plugin starts.
- **Startup checks are not comprehensive runtime monitoring.** Unverified is not a confirmed failure; passing checks does not validate every conversation, tool or third-party plugin. First-run preparation still depends on storage, configuration and plugin count.
- **The OS can suppress notifications.** Windows notification settings or Do Not Disturb may hide balloons. Workbench and tray retain the status. User acceptance on Windows does not establish real-device notification acceptance on macOS or Linux.
- **Platform scope and offline behavior are unchanged.** Official Desktop remains available on Windows x64 and macOS Intel/Apple Silicon; Windows ARM64 and Linux ARM64 provide Web mode. Linux ARM64 offers AppImage, DEB and RPM without claiming real-device acceptance on every distribution. Nexus includes its runtimes; preparation after acquiring Harness or importing a full offline package on the same OS and architecture needs no network. Downloading third-party plugins still requires a connection.

## 下载与验证 / Downloads and verification

Windows x64/ARM64：EXE 或 ZIP；macOS Intel/Apple Silicon：DMG 或 ZIP；Linux ARM64：AppImage、DEB、RPM。ZIP 必须完整解压，不能只复制可执行文件；免安装不代表用户数据随目录移动。

Windows x64/ARM64: EXE or ZIP; macOS Intel/Apple Silicon: DMG or ZIP; Linux ARM64: AppImage, DEB or RPM. Extract ZIP packages completely; do not copy only the executable. Portable installation does not make user data portable.

以同一提交的五平台 Actions 结果、附件内的 `_build.json` 和 SHA256 清单确认构建来源与自动化检查范围。摘要签名不是操作系统代码签名；构建和安装包冒烟不等于所有系统、完整业务、输入法和自动更新的真机验收。本地测试中显式跳过的环境相关用例不计入通过数量。

Use the five-platform Actions results for the same commit, packaged `_build.json` files and SHA256 manifests to verify provenance and automated check scope. Manifest signing is not OS code signing. Builds and package smoke checks do not establish device acceptance for every OS, workflow, IME or automatic-update scenario. Environment-specific tests explicitly skipped locally are not counted as passed.
