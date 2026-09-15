# GitHub 构建与发布

五架构 CI 已在提交 `03a7922` 全部通过：[构建与安装启动验证](https://github.com/chelal233/dsh-nexus/actions/runs/34932049352)。`v0.1.2` 复用该次产物发布下载，不重新编译。CI 启动验证不等于完整 Harness 真机验收。

## 平台矩阵

| 目标 | GitHub runner | 产物 | 内置运行时 |
| --- | --- | --- | --- |
| `x86_64-pc-windows-msvc` | `windows-2022` | 多语言 NSIS EXE | Node 24.20.0 x64 |
| `i686-pc-windows-msvc` | `windows-2022` | 多语言 NSIS EXE | Node 22.23.2 x86 |
| `aarch64-pc-windows-msvc` | `windows-11-arm` | NSIS EXE | Node 24.20.0 ARM64 |
| `x86_64-apple-darwin` | `macos-15-intel` | DMG | Node 24.20.0 x64 |
| `aarch64-apple-darwin` | `macos-15` | DMG | Node 24.20.0 ARM64 |

所有目标内置 pnpm 11.7.0。Node 24 官方无 Windows x86 发行包，故此目标单独固定 Node 22；不修改 Harness 的版本要求。所有 Windows 架构统一提供一个 NSIS EXE，安装时选择简体中文或 English，不再生成 MSI。macOS 分架构发行，不将单架构 Node 和辅助程序放入所谓 Universal 包。macOS 无现代 32 位 x86 产品；Linux、ARM32、其他 CPU 不在本次矩阵内。

Windows x86 在 x64 runner 上编译并运行 x86 Rust 测试及内置 Node 探针；不能等同于 Windows 10 32 位机器验收。其他目标使用匹配架构 runner。脚本拒绝不受支持的交叉构建，避免错装宿主架构运行时。

## 开发机与终端用户要求

- 编译：Rust 1.98.0，构建用 Node 24.20.0，pnpm 11.7.0；依赖用锁文件安装。
- Windows：MSVC C++ Build Tools、Windows SDK、目标架构 C++ 组件。CI 工具安装只作用于临时 runner。
- macOS：Xcode Command Line Tools；应用最低系统版本为 macOS 13.5，同时覆盖内置 Node 的要求。
- 用户：安装预编译包，不需要 Rust、Xcode、MSVC、系统 Node/pnpm。Windows 使用包内 WebView2 离线安装器；macOS 使用系统 WKWebView。受管 Harness 首次安装仍可能联网安装依赖并构建，其原生依赖失败会保留原始错误。
- Windows 未配置 Authenticode；macOS 使用 ad-hoc 签名，未配置 Developer ID 与公证。面向普通用户的签名发布需要维护者证书和独立验收；当前产物定位为开发预发布，不自动提供绕过平台保护的操作。
- 离线安装是发行要求，发布工作流强制检查 Windows 使用 `offlineInstaller`；不以减小包体积为由改回联网安装 WebView2。Harness 离线使用仍需预先准备对应离线材料。

## 工作流

`Desktop build` 在 main push、PR 和手动触发时执行五个目标。步骤包括前端检查、目标架构 Rust 测试、兼容性检查、辅助程序编译、匹配架构 Node/npm/pnpm 准备与嵌套命令测试、许可材料收集、原生桥接测试、Tauri 打包、资源校验与附件收集。矩阵一项失败不会取消其他项，但整体不允许生成 Release 草稿。PR 仅有只读仓库权限，不使用发布密钥。

`Publish release` 在 `v*` 标签 push 后自动提供 Release 下载；标签必须精确等于 `v<package version>`。先查找同一提交已成功且包含全部五架构、未过期附件的 Desktop build；找到则直接复用，否则执行完整矩阵。随后验证五个目标的版本、提交、安装启动结果、安装包数量和 SHA-256，上传全部附件到草稿，上传成功才发布为 prerelease。已有同名 Release 不自动覆盖，失败后应先检查草稿和附件。普通构建保留 Actions artifacts 14 天；Release 附件供用户分别下载，不要求下载整个 Actions ZIP。私有仓库的 Release 下载仍需要仓库访问权限。

日常开发只需运行必要的目标；发布时推送版本 tag 即包含上传和发布下载流程。复用要求完全相同的提交，不能用旧提交的产物冒充新 tag。发布工作流自身仍消耗少量 Linux runner 时间；额度耗尽时不要反复触发。首次 `v0.1.2` 通过本地 GitHub CLI 发布已验证产物，临时停用旧的 tag 工作流以避免重复构建，完成后恢复。

产物文件名采用 `dsh-nexus_<版本>_<系统>_<架构>.<扩展名>`；系统为 `windows` 或 `macos`，架构为 `x86`、`x64` 或 `arm64`。仅当实际按 CPU 子型号构建时才增加子架构字段。每个平台附同名前缀的 `_SHA256SUMS.txt` 和 `_build.json`，精确 Rust target 和唯一 run/attempt 构建编号保留在元数据内，只含版本、提交、构建编号、运行时版本、签名/机器验收状态及哈希；不上传完整测试日志或诊断目录。

## 本地执行

Windows PowerShell 示例（可将目标替换为 `i686-pc-windows-msvc`；ARM64 请在 ARM64 开发机执行）：

```powershell
cd apps/nexus-launcher
pnpm install --frozen-lockfile
$env:CARGO_BUILD_TARGET = 'x86_64-pc-windows-msvc'
$env:NEXUS_BUILD_ID = 'local-' + (Get-Date -Format 'yyyyMMddHHmmss')
rustup target add $env:CARGO_BUILD_TARGET
pnpm tauri build --target $env:CARGO_BUILD_TARGET --bundles nsis -- --locked
```

ARM64 将 `--bundles` 改为 `nsis`。macOS 原生示例：

```bash
cd apps/nexus-launcher
pnpm install --frozen-lockfile
export CARGO_BUILD_TARGET=aarch64-apple-darwin # Intel 使用 x86_64-apple-darwin
export NEXUS_BUILD_ID=local-$(date +%Y%m%d%H%M%S)
export MACOSX_DEPLOYMENT_TARGET=13.5
rustup target add "$CARGO_BUILD_TARGET"
pnpm tauri build --target "$CARGO_BUILD_TARGET" --bundles dmg -- --locked
```

构建自动准备辅助程序、运行时、许可材料、版本身份及前端。使用显式 target 时，安装包位于 Cargo target 目录的 `<target>/release/bundle/`。`CARGO_TARGET_DIR` 可指定独立输出目录。`collect-release.mjs` 只用于检查已通过的干净提交构建，拒绝 dirty checkout；它本身不执行测试。

原有 `pnpm release:gate` 保留为 **Windows x64 本地门禁**，使用前清除 `CARGO_BUILD_TARGET` 和 `NEXUS_BUILD_ID` 环境变量。它与跨平台 CI 的包收集流程分开；CI 会执行 NSIS 静默安装/卸载、DMG 挂载复制、包内哈希校验、Agent/CLI 身份验证和 GUI 进程启动检查；这仍不代表真实用户交互、Gatekeeper/SmartScreen 信任或完整 Harness 真机验收。

## 已知平台功能差异

DSH 交互终端入口及终端租约目前仅在 Windows 实现；macOS 调用会返回 `terminal_unsupported`，尚不具备此入口的功能对等性。自动构建和包启动成功不能抹去此限制。

## 首次发布操作

1. 审核当前文件和 Git 历史，尤其是 `artifacts/takeover`、内部文档、本机路径和历史诊断。`.gitignore` 不会清除已跟踪文件或历史；不使用 `git push --mirror`。
2. 将审核后的提交放入计划发布的 main，配置正确 GitHub remote，启用 Actions；确认 ARM runner 可用于该仓库和账户。设置所需分支保护及私密漏洞报告入口。
3. 先执行 Desktop build，排除任何目标失败；核对生成的 `notices/components.json`，补齐 `reviewRequired` 项及嵌套组件许可义务。
4. 对每个目标完成安装、首次启动、Agent 身份、Harness 安装/启动/停止、升级、卸载及数据保留验收；macOS 增查 DMG 挂载、复制到 Applications 后启动、资源可执行权限和系统权限提示。使用 [现有验收清单](manual-acceptance-0.1.2.md) 并记录平台差异。
5. 同步根 Cargo、GUI Cargo、两个 Cargo.lock 中本项目包版本、package.json 和 tauri.conf.json；版本必须与发布 tag 一致。对已审核提交创建对应标签并显式 push 标签。
6. 打 tag 前更新 `.github/RELEASE_TEMPLATE.md` 中的下载说明、验证范围和已知问题。工作流成功后自动发布预发布版及各架构独立下载附件，维护者核对 Release 结果；稳定版发布需独立确认验收与签名状态。

校验下载文件：Windows 使用 `Get-FileHash <安装包> -Algorithm SHA256`；macOS 在附件目录执行 `shasum -a 256 -c dsh-nexus_<版本>_<系统>_<架构>_SHA256SUMS.txt`。

参考：[GitHub runner 矩阵](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)、[Tauri Windows 打包](https://tauri.app/distribute/windows-installer/)、[Tauri macOS bundle](https://tauri.app/distribute/macos-application-bundle/)、[Node 24 校验清单](https://nodejs.org/dist/v24.20.0/SHASUMS256.txt)、[Node 22 x86 校验清单](https://nodejs.org/dist/v22.23.2/SHASUMS256.txt)。

## 本次本机验证（2026-09-15）

- TypeScript 检查、前端生产构建、114 项前端测试通过；构建仍有既存重复 case 和体积提示。
- 18 项发布脚本测试通过；Windows x64 Node 24 与 x86 Node 22 的无系统开发工具 PATH 嵌套命令测试分别通过。
- Windows x64 三个辅助程序 Release 编译与静态 CRT/PE 导入检查通过；2,986 项资源清单生成和复核通过。
- actionlint 1.7.12 通过；两平台合并配置通过 Tauri schema 结构校验（不代替原生打包）。
- 许可收集生成 575 项记录，40 项 `reviewRequired` 待核对。
- 后续 GitHub 五架构矩阵已全部通过完整编译、Rust 回归与 CI 安装启动检查；九个安装文件已下载且 SHA-256 一致。完整用户交互、Harness 工作流和升级数据保留验收仍待执行。
