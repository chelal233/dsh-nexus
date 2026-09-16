# Electron 发布

唯一桌面宿主为 Electron 44；构建矩阵为 Windows x64、Windows ARM64、macOS x64、macOS ARM64。Windows 使用 NSIS EXE，macOS 使用 DMG 与自更新 ZIP。32 位构建已移除。所有桌面包内置 Chromium 和独立 Node/npm/pnpm，用户无须安装开发工具。

## 本地构建

在 `apps/nexus-launcher` 执行：

```sh
pnpm install --frozen-lockfile
node node_modules/electron/install.js
pnpm prepare:agent
pnpm prepare:runtime
pnpm test:bundled-runtime
pnpm prepare:notices
pnpm prepare:release
pnpm electron:build
pnpm verify:release
```

正式构建默认要求签名与 macOS 公证。`NEXUS_UNSIGNED_SMOKE=1` 只生成验收包；不代表签名升级已经通过。

## CI 与发布

`build.yml` 是唯一桌面构建入口。四个原生 runner 执行 Rust/前端测试、离线运行时校验、Electron 打包、安装后资源与真实渲染器 smoke，最后收集安装包、每架构更新元数据、构建来源与校验值。

`release.yml` 只复用同提交的完整成功矩阵；不完整时重建。发布前校验全部四个平台与附件哈希。附件名称为 `dsh-nexus_<version>_<windows|macos>_<x64|arm64>`；更新通道文件按平台和架构区分，避免相互覆盖。

当前版本为 0.1.4。发布 tag 必须为 `v<package.json 版本>`，并与根 Cargo.toml、Cargo.lock 中本项目版本保持一致，不覆盖已发布 tag。CI 的 unsigned/ad-hoc 产物作为预发布验收包，不代表签名自动升级已验证。

更新源为 GitHub Releases。启用自动更新时启动检查一次、之后每两小时检查；设置中的“检测更新”不受自动更新开关限制。发现新版本后后台全量下载，左下角显示进度；校验通过后“更新”按钮才允许重启安装。中途退出的半包不参与安装；重新启动先检测最新发布 tag，缓存只有通过最新版本的校验信息才允许复用，不保证断点续传。
