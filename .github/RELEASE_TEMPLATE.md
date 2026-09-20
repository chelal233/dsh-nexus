# Nexus Launcher

## 下载 / Downloads

Windows x64/ARM64：EXE 安装包或 ZIP 免安装包。macOS Intel/Apple Silicon：DMG 或 ZIP。Linux ARM64：AppImage、DEB、RPM（仅列入已通过原生构建和启动检查的附件）。无 32 位版本。

Windows x64/ARM64: EXE installer or portable ZIP. macOS Intel/Apple Silicon: DMG or ZIP. Linux ARM64: AppImage, DEB and RPM (include only artifacts that passed native build and launch checks). No 32-bit builds.

Windows ZIP 请完整解压并运行 `Nexus Launcher.exe`，不能单独复制 EXE。免安装不等于数据便携：用户配置与 Harness 数据仍保留在用户数据目录。

Extract the entire Windows ZIP and run `Nexus Launcher.exe`; do not copy the EXE alone. Portable means no installer is required, not that user configuration and Harness data move with the application folder.

包内含 Chromium、Rust Agent、Node/npm/pnpm，以及支持平台的官方 Desktop 离线运行时。取得 Harness 版本后，Desktop 准备与完整离线包导入均不需要联网。官方 Desktop 支持 Windows x64、macOS Intel／Apple Silicon；Windows ARM64 和 Linux ARM64 当前提供 Web 模式。兼容范围取 Harness 与 Electron 的交集。

Includes Chromium, Rust Agent, Node/npm/pnpm and the official Desktop offline runtime on supported platforms. After acquiring Harness, Desktop preparation and full offline-package import need no network. Official Desktop supports Windows x64 and macOS Intel/Apple Silicon; Windows ARM64 and Linux ARM64 currently provide Web mode. Supported targets are the intersection of Harness and Electron support.

## 本次变化 / Changes in this release

- 统一 Harness 工作台与托盘操作，直接运行官方 Desktop；启动失败提供有证据的修复建议。
- 共享 Electron 并预组装离线依赖；完整离线包保留同系统、同架构运行所需的依赖。
- 补齐 macOS 签名校验，新增 Linux ARM64 的 AppImage、DEB、RPM；Linux 使用 Web Harness，上游未支持的 Desktop 不显示入口。

- Unify Harness controls and launch the official Desktop with evidence-based startup repair.
- Share Electron and preassemble offline dependencies; full exports remain self-contained for the same OS and architecture.
- Verify signed macOS resources and add Linux ARM64 AppImage, DEB and RPM packages. Linux uses Web Harness; unsupported upstream Desktop actions stay hidden.

## 验证 / Verification

以本次 `_build.json`、SHA256 清单和 Actions 结果确认来源及检查范围。清单签名不等于操作系统代码签名；自动化检查不等于完整业务、更新、输入法与跨平台真机验收。

Use this release's `_build.json`, SHA256 manifests and Actions results to establish provenance and check scope. Manifest signatures are separate from OS code signing. Automation does not establish complete workflow, update, IME or cross-platform device acceptance.

更新会重启 Nexus；运行中的 Harness 会被中止，请先保存工作。可手动下载完整包使用，无需通过自动更新。

Applying an update restarts Nexus and stops a running Harness; save work first. You may download a complete package manually without using automatic updates.
