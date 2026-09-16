# Nexus Launcher

Nexus 是面向 Windows 和 macOS 的 Harness 启动器，提供安装与版本选择、启动检查、profile 管理、运行配置补丁、恢复辅助和故障诊断。界面支持简体中文和 English。

**当前为 0.1.3 候选版本。自动化检查、包内校验与真实机器验收分别记录；尚未执行的验收不算通过。** 发布后的安装包位于本仓库 Releases，源码目录不等于安装包。

## 安装与第一次启动

按操作系统和 CPU 架构选择附件。下表列出发行构建目标；每个版本的自动化验证结果以对应 Actions 记录为准，各架构实际机器验收单独记录。

| 系统 | CPU | 安装包 | 内置 Node | 系统要求 |
| --- | --- | --- | --- | --- |
| Windows | x64 / AMD64 | EXE（安装时选择语言） | 24.20.0 | Windows 10/11 x64 |
| Windows | ARM64 / aarch64 | EXE | 24.20.0 | Windows 11 ARM64 |
| macOS | Intel x64 | DMG | 24.20.0 | macOS 13.5 或更新 |
| macOS | Apple Silicon ARM64 | DMG | 24.20.0 | macOS 13.5 或更新 |

安装包统一命名为 `dsh-nexus_<版本>_<系统>_<架构>.<扩展名>`，例如 `dsh-nexus_0.1.3_windows_x64.exe`。构建编号和精确 Rust target 保留在同名前缀的 `_build.json` 中。

发行目标为 Windows/macOS x64 和 ARM64，不再提供 32 位版本。Electron 自带 Chromium，不依赖系统 WebView。当前本地包用于验收，跨平台与签名更新结果以对应构建记录为准。

1. 安装并打开 Nexus，确认 Agent 正常。
2. 在引导页安装一个受支持的 Harness 版本，或选择自己已经准备好的外部 Harness 程序目录。
3. 按需选择 Harness 数据目录与 profile；留空的可选设置沿用原始配置，不进行数据迁移。
4. 执行启动检查，处理阻断项，再启动 Harness。
5. 在工作站打开 Harness Web 或可输入命令的 DSH 终端。托盘也提供常用控制入口。

Nexus 安装、升级和卸载不会现场编译。内置 Node/npm/pnpm 供 Harness 安装构建与运行使用；Nexus 的 Git 操作可使用内嵌实现，不要求普通用户预装开发工具。首次安装受管 Harness 允许联网下载、安装依赖和构建；上游自身的原生依赖问题仍可能导致失败，Nexus 不自动安装编译器，且保留原始错误。已准备好的版本启动不应再次下载或构建。

## 程序与数据边界

| 内容 | Nexus 的职责 |
| --- | --- |
| 受管版本槽 | Nexus 安装、切换、回退和按明确范围清理 |
| 外部 Harness 程序目录 | 只保存路径并检查、启动；不复制、构建、更新、切分支或删除 |
| Harness 数据目录（`DSH_HOME`） | 独立配置访问路径，不自动迁移数据 |
| 项目工作目录 | 使用 Harness 自身的会话/工作区能力，与程序目录、`DSH_HOME` 分开 |
| 运行配置补丁 | 通过 `--patch` 叠加；用户控制来源、版本、启用状态与顺序，不修改上游源码 |

外部 Harness 自身及插件仍可能写文件；Nexus 只读程序目录的约定不是操作系统沙箱。外部目录未准备好或发生变化时会重新检查，不会自动修复它。启用的补丁失败会阻止 Harness 启动，用户仍可进入设置，修复或主动禁用对应补丁。

普通卸载与“同时清理 Nexus 数据”含义不同：清理应用数据可能移除 Nexus 管理的版本和记录。解除外部目录关联不会删除原目录。快照只恢复明确声明的范围，不能代替项目、会话及全部用户数据的独立备份。

## 日常操作与恢复

- 关闭窗口会隐藏到托盘；退出界面与停止服务退出由不同菜单操作控制。
- 长时间的启动准备可取消，界面会等待检查进程安全退出。重启已经停止旧实例时，取消不会自动把旧实例重新启动。
- 恢复模式暂停 Harness 启动，Agent 仍应正常运行；修复设置后重新检查，再由用户启动。
- Agent 不可用时仍可尝试 Launcher 独立诊断。损坏的历史诊断会保留并告警，不应阻断其他健康记录。
- 原始错误、构建编号和操作步骤是反馈依据；提交诊断前自行检查，不要在公开 Issue 粘贴密钥、访问令牌或未经检查的完整日志。

## 开发与验证

开发者需要 Rust 1.98.0、Node 24.20.0、pnpm 11.7.0；Windows 使用 MSVC C++ 工具及对应架构组件，macOS 使用 Xcode Command Line Tools。这些是编译条件，不是用户安装 Nexus 的前提。

```powershell
cd apps/nexus-launcher
pnpm install --frozen-lockfile
pnpm typecheck
pnpm test
pnpm test:rust
$env:NEXUS_BUILD_ID = 'dev-' + (Get-Date -Format 'yyyyMMddHHmmss')
pnpm prepare:agent
pnpm prepare:runtime
pnpm prepare:notices
pnpm prepare:release
pnpm dev
```

Windows x64 本地完整门禁：在 `apps/nexus-launcher` 执行 `pnpm release:gate`。结果写入仓库 `target-rtest/release/verify-<构建编号>`，候选安装包保存在对应 `verify-<构建编号>/attempt-*` 验证目录中，避免同版本的多次构建互相覆盖。完整记录可能含本机路径和测试输出，**不要整目录上传到公开 Release**。

运行完整发布门禁前，在开发终端执行 `Remove-Item Env:NEXUS_BUILD_ID -ErrorAction SilentlyContinue`，让门禁生成新的候选构建编号。原生测试同样依赖上面的资源准备步骤；新克隆不能依赖旧机器上的缓存。

- [开发说明](apps/nexus-launcher/README.md)
- [手动验收清单](docs/manual-acceptance-0.1.2.md)
- [公开发布准备](docs/github-release.md)
- [贡献约定](CONTRIBUTING.md)
- [安全问题报告](SECURITY.md)

Nexus 自有代码沿用仓库声明的 [MIT 许可证](LICENSE)。第三方组件各自适用其许可证，见 [第三方材料说明](THIRD_PARTY_NOTICES.md)。Nexus 是独立启动器，不代表上游 Harness 官方发布。
