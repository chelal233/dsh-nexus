# Nexus Launcher

## 下载 / Downloads

Windows x64/ARM64：EXE 安装包或 ZIP 免安装包。macOS Intel/Apple Silicon：DMG 或 ZIP。Linux ARM64：AppImage、DEB、RPM（仅列入已通过原生构建和启动检查的附件）。无 32 位版本。

Windows x64/ARM64: EXE installer or portable ZIP. macOS Intel/Apple Silicon: DMG or ZIP. Linux ARM64: AppImage, DEB and RPM (include only artifacts that passed native build and launch checks). No 32-bit builds.

Windows ZIP 请完整解压并运行 `Nexus Launcher.exe`，不能单独复制 EXE。免安装不等于数据便携：用户配置与 Harness 数据仍保留在用户数据目录。

Extract the entire Windows ZIP and run `Nexus Launcher.exe`; do not copy the EXE alone. Portable means no installer is required, not that user configuration and Harness data move with the application folder.

包内含 Chromium、Rust Agent、Node/npm/pnpm，以及支持平台的官方 Desktop 离线运行时。取得 Harness 版本后，Desktop 准备与完整离线包导入均不需要联网。官方 Desktop 支持 Windows x64、macOS Intel／Apple Silicon；Windows ARM64 和 Linux ARM64 当前提供 Web 模式。兼容范围取 Harness 与 Electron 的交集。

Includes Chromium, Rust Agent, Node/npm/pnpm and the official Desktop offline runtime on supported platforms. After acquiring Harness, Desktop preparation and full offline-package import need no network. Official Desktop supports Windows x64 and macOS Intel/Apple Silicon; Windows ARM64 and Linux ARM64 currently provide Web mode. Supported targets are the intersection of Harness and Electron support.

<!-- 每次发布必须按 docs/github-release.md 的日志标准重写下列双语内容，不沿用上一版本条目。 Rewrite both languages for each release following docs/github-release.en.md; never reuse stale entries. -->

## 本次变化（中文）

1. **直接使用 Harness 官方桌面端**

   现在可以启动当前受管 Harness 版本自带的 Desktop，无需使用 Nexus 自制客户端。只有所选版本与当前平台支持时才显示桌面入口，避免出现无法使用的选项。

2. **工作台和托盘更清晰，配置切换更方便**

   Web 与 Desktop 统一放在 Harness 区域，明确显示当前模式、运行状态和可用操作，并保留醒目的配置切换入口。托盘补齐桌面端启动、Web／Desktop 中止、配置与维护入口，同时区分“仅退出启动器”和“中止所有服务后退出”。

3. **启动失败后有明确的修复路径**

   区分真正出错的插件、缺失服务和仍在等待的插件，根据明确证据提供修复建议。用户确认后可暂时禁用相关插件并重新检查、启动；插件包和数据保留，后续可以重新启用。

4. **启动结果更可信**

   不再仅凭进程运行或网页可访问就显示启动成功，而是检查客户端插件和核心服务，并区分检查中、功能受限、启动失败和未验证。加载长期阻塞时也会持续提供状态；这些检查针对启动阶段，不代表所有会话、工具或运行中业务都已验证。

5. **减少重复文件，改善首次启动等待**

   Nexus 与官方 Desktop 共用匹配版本的 Electron，桌面端依赖提前准备，重复启动可复用已校验的本地文件。准备界面显示阶段、耗时和取消入口，用户能够了解进度并中止等待；准备期间无需联网补装依赖。

6. **完整离线包更可靠，移动目录后更省心**

   完整导出包含匹配的 Harness 和所需运行时，同系统、同架构导入后无需联网补装。修复便携目录移动后的旧路径引用，并保留 Unix 可执行权限与相对链接；仅配置／数据导出仍属于部分包，不能替代完整离线包。

7. **增加 Linux ARM64 下载，完善跨平台交付**

   新增 AppImage、DEB、RPM，Windows 与 macOS 继续提供对应架构的安装包和压缩包。五个平台均通过 CI 与安装包检查；Windows ARM64 和 Linux ARM64 当前提供 Web Harness，官方 Desktop 仅在上游与 Electron 均支持时开放。安装包检查不代表已逐一验证所有 Linux 发行版和业务场景。

## What changed (English)

1. **Use the official Harness Desktop**

   Launch the Desktop included in your selected managed Harness release instead of a Nexus-built replacement client. The Desktop option appears only when both the selected release and your platform support it.

2. **Clearer workbench and tray controls, with easier profile switching**

   Web and Desktop now share one Harness area with clear mode, status and available actions, while profile switching remains easy to find. The tray adds Desktop launch, separate Web/Desktop stop controls, profile and maintenance shortcuts, and distinct options to exit only the launcher or stop all services.

3. **A practical recovery path when startup fails**

   Nexus distinguishes failing plugins, missing services and plugins still waiting for dependencies, then recommends repairs based on observed evidence. With your confirmation, it can temporarily disable the relevant plugins and check and start again. Packages and data remain available, and plugins can be re-enabled later.

4. **More reliable startup results**

   A running process or accessible webpage no longer counts as successful startup on its own. Nexus checks client plugins and core services, showing checking, limited functionality, startup failure or unverified states, including when loading remains blocked. These checks cover startup, not every conversation, tool or operation during use.

5. **Fewer duplicate files and less first-start preparation**

   Nexus and official Desktop share a matching Electron runtime, with Desktop dependencies prepared in advance and verified local files reused on later launches. Preparation shows its stage, elapsed time and a cancel action, so you can follow progress or stop waiting. It does not download missing dependencies during preparation.

6. **More dependable offline packages and portable directory moves**

   Full exports include matching Harness files and required runtimes, allowing import on the same OS and architecture without downloading dependencies. This release fixes stale paths after moving a portable directory and preserves Unix executable permissions and relative links. Configuration/data-only exports remain partial packages and do not replace a full offline package.

7. **Linux ARM64 downloads and broader platform delivery**

   AppImage, DEB and RPM packages join the existing Windows and macOS installers and archives. All five targets passed CI and package checks. Windows ARM64 and Linux ARM64 currently offer Web Harness; official Desktop appears only where both upstream Harness and Electron support it. Package checks do not establish compatibility with every Linux distribution or business workflow.

## 验证 / Verification

以本次 `_build.json`、SHA256 清单和 Actions 结果确认来源及检查范围。清单签名不等于操作系统代码签名；自动化检查不等于完整业务、更新、输入法与跨平台真机验收。

Use this release's `_build.json`, SHA256 manifests and Actions results to establish provenance and check scope. Manifest signatures are separate from OS code signing. Automation does not establish complete workflow, update, IME or cross-platform device acceptance.

更新会重启 Nexus；运行中的 Harness 会被中止，请先保存工作。可手动下载完整包使用，无需通过自动更新。

Applying an update restarts Nexus and stops a running Harness; save work first. You may download a complete package manually without using automatic updates.
