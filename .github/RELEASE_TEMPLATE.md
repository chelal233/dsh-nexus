# Nexus Launcher

## 下载 / Downloads

Windows x64/ARM64：EXE 安装包或 ZIP 免安装包。macOS Intel/Apple Silicon：DMG 或 ZIP。无 32 位版本。

Windows x64/ARM64: EXE installer or portable ZIP. macOS Intel/Apple Silicon: DMG or ZIP. No 32-bit builds.

Windows ZIP 请完整解压并运行 `Nexus Launcher.exe`，不能单独复制 EXE。免安装不等于数据便携：用户配置与 Harness 数据仍保留在用户数据目录。

Extract the entire Windows ZIP and run `Nexus Launcher.exe`; do not copy the EXE alone. Portable means no installer is required, not that user configuration and Harness data move with the application folder.

包内含 Chromium、Rust Agent 与 Node/npm/pnpm。Harness 首次在线依赖准备不属于桌面离线安装。

Includes Chromium, Rust Agent and Node/npm/pnpm. Initial online Harness dependency preparation is separate from offline desktop installation.

## 本次变化 / Changes in this release

<!-- 填写实际产品变化、已知限制和修复版本。Fill in actual changes, known limitations and fixed versions. -->

## 验证 / Verification

以本次 `_build.json`、SHA256 清单和 Actions 结果确认来源及检查范围。清单签名不等于操作系统代码签名；自动化检查不等于完整业务、更新、输入法与跨平台真机验收。

Use this release's `_build.json`, SHA256 manifests and Actions results to establish provenance and check scope. Manifest signatures are separate from OS code signing. Automation does not establish complete workflow, update, IME or cross-platform device acceptance.

更新会重启 Nexus；运行中的 Harness 会被中止，请先保存工作。可手动下载完整包使用，无需通过自动更新。

Applying an update restarts Nexus and stops a running Harness; save work first. You may download a complete package manually without using automatic updates.
