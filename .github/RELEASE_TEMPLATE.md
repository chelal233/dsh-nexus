Nexus Launcher 开发预发布。

## 下载选择

- Windows x64：选择 `x86_64-pc-windows-msvc` 的 `setup.exe`。
- Windows x86：选择 `i686-pc-windows-msvc` 的 `setup.exe`。
- Windows ARM64：选择 `aarch64-pc-windows-msvc` 的 `setup.exe`。
- macOS Intel：选择 `x86_64-apple-darwin` 的 `.dmg`。
- macOS Apple Silicon：选择 `aarch64-apple-darwin` 的 `.dmg`。

每次只需下载匹配系统的一个安装包。Windows 每个架构只提供一个 EXE，运行后可选择简体中文或 English，不再按语言分开发包。Windows 包含 WebView2 离线安装器及 Node/npm/pnpm，体积大于单独的程序。Windows x86 内置 Node 22，其余内置 Node 24。macOS 最低版本为 13.5。

离线安装是发行要求：Windows 保持 `offlineInstaller`，不依赖安装时联网下载 WebView2。Nexus 自身可离线安装；Harness 的离线安装、恢复和运行需要预先准备对应离线材料，不能将空白机器上的首次在线获取依赖称为完全离线。

## 验证与限制

五个架构均通过编译、自动化回归、安装包资源哈希校验、Agent/CLI 通信及 GUI 进程启动检查。每个平台附 `SHA256SUMS.txt` 和 `build.json`，记录来源提交、构建编号、运行时和校验值。

CI 不代表完整 Harness 业务、升级、数据保留或用户交互真机验收。Windows 未商业签名；macOS 仅 ad-hoc 签名，未 Apple 公证。macOS 的 DSH 交互终端尚未实现。第三方许可材料已包含在安装包内，仍有 `reviewRequired` 项待核对。本版本不声明稳定版或各平台功能完全对等。
