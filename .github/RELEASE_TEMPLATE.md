# Nexus Launcher

Electron 桌面包支持 Windows/macOS x64 和 ARM64；不再提供 32 位版本。
Windows 可选择 EXE 安装包或 ZIP 免安装包（x64 / ARM64）；ZIP 完整解压后运行 `Nexus Launcher.exe`，不需要执行安装程序。请保留整个解压目录，不能单独复制 EXE。
macOS 可选择 DMG 或 ZIP 应用包。`latest` 元数据用于自动更新。

免安装指程序无需安装；用户配置与 Harness 数据仍保存在现有用户数据目录，不会随 ZIP 自动迁移。免安装包也可以手动下载新版、解压到新目录，退出旧版 Launcher 后运行新版。

包内包含 Chromium、Rust Agent 和 Node/npm/pnpm，无须预装开发工具。
Harness 的首次在线依赖准备与桌面离线安装不同。

请以本次 `_build.json`、SHA256 清单及 Actions 结果确认来源、签名和验证状态。
自动化检查不等于完整业务、签名升级、输入法和跨平台真机验收。
