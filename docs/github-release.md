# 构建与发布

[English](github-release.en.md)


当前使用 Electron，发行矩阵为 Windows x64/ARM64 和 macOS x64/ARM64。没有 Tauri、WebView2 安装器、x86 或第五个目标。

## 两个工作流

`Desktop build`：前端/Rust/Electron 检查 → 目标架构辅助程序与运行时 → 许可和资源清单 → 打包 → 安装/解压冒烟 → 收集附件。

`Publish release`：在 `v*` 标签上运行，要求标签等于 `v<package version>`；复用同一提交、完整成功的四目标构建，否则运行构建矩阵。验证附件后生成签名校验清单，创建草稿、上传，全部成功后公开为预发布版。

源码推送、tag 创建、构建通过、Release 公开是四种不同状态。已有草稿时不要盲目重跑创建步骤；先检查附件。不要移动已发布 tag 或拿其他提交的包补充该版本。

## 发布前

1. 同步根 Cargo.toml、Cargo.lock 中 Nexus 包版本和 Launcher package.json；更新双语变更记录。
2. 在匹配目标的原生平台构建，确认依赖锁文件、运行时摘要和许可材料。
3. 执行工作流规定的测试和安装/解压检查；另行记录真实用户交互验收。
4. 检查 EXE/DMG、ZIP、架构更新 YAML、`_build.json`、逐架构 SHA256 文件齐全；检查包内 `resources/app-update.yml`。
5. 确认同一提交的四目标成功，再推送版本标签。查看发布任务和 Release，不把普通构建成功当成发布完成。

## 本地打包

先按[开发说明](../apps/nexus-launcher/README.md)准备资源；在 Launcher 目录执行：

```sh
pnpm electron:build
```

输出在 `apps/nexus-launcher/electron-dist`。仅本地无签名验收时显式设置 `NEXUS_UNSIGNED_SMOKE=1`。Windows x64 完整门禁使用 `pnpm release:gate`，先清除旧的 `NEXUS_BUILD_ID` 和 `CARGO_BUILD_TARGET`；不要把门禁日志目录当成公开附件上传。

## 附件与校验

文件名：`dsh-nexus_<version>_<windows|macos>_<x64|arm64>.<exe|dmg|zip>`。当前四目标完整 Release 有 20 个构建附件，加聚合 SHA256 清单、签名和证书，共 23 个附件。签名方法见[安全说明](../SECURITY.md)。

Windows：`Get-FileHash <文件> -Algorithm SHA256`。macOS：`shasum -a 256 -c <架构>_SHA256SUMS.txt`。自动校验脚本 `.github/scripts/verify-release-assets.mjs` 核对版本、提交、目标与摘要。

## 上传中断

保留草稿；核对服务器已有文件的状态、大小和 SHA-256，仅补传缺失的同一构建产物，不覆盖正确附件。全部核对完成后再公开。手动补传成功不会把原本失败或取消的 Actions 记录变绿，应在交付中说明。

CI 编译、安装冒烟、系统签名信任、自动升级及完整业务验收分别记录。v0.1.7 发布时四目标构建成功，上传中断后补齐附件完成发布；这不代表所有真机流程都已验收。
