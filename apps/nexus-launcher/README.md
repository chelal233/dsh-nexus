# Launcher 开发

[English](README.en.md)


需要 Rust 1.98.0、Node 24.20.0、pnpm 11.7.0。Windows 使用 MSVC C++ 工具链；macOS 使用 Xcode Command Line Tools。具体版本以工作流和锁文件为准。

## 本地启动

```sh
cd apps/nexus-launcher
pnpm install --frozen-lockfile
node node_modules/electron/install.js
pnpm typecheck
pnpm test
pnpm test:electron
pnpm prepare:agent
pnpm prepare:runtime
pnpm prepare:notices
pnpm prepare:release
pnpm dev
```

这些命令从仓库根目录开始。资源准备会编译辅助程序并下载相应平台运行时；首次运行可能较慢。`pnpm dev` 打开 Electron；`pnpm dev:web` 仅是 React 浏览器预览，原生文件选择、托盘和自动更新在其中不可验收。

## 构建与测试

`pnpm build` 执行类型检查和 Vite 构建。`pnpm electron:build` 打包，输出位于 `electron-dist`。`pnpm release:gate` 是 Windows x64 本地完整门禁；其输出与跨平台 CI 不同。测试资源必须与目标架构匹配。

生产打包默认要求代码签名。`NEXUS_UNSIGNED_SMOKE=1` 只用于明确标记的开发/验收包，不代表产物已签名或公证。发布细节见[发布流程](../../docs/github-release.md)。

设置 `NEXUS_DATA_DIR` 和 `DSH_HOME` 为独立测试目录，避免触碰真实数据。`NEXUS_SMOKE_EXECUTABLE` 可指定安装版或解压后的 Electron 可执行文件；冒烟通过不证明全部业务交互通过。

源码入口：`src/App.tsx`、`electron/main.mjs`、`electron/preload.cjs`，构建与资源脚本位于 `desktop/scripts`。Rust 使用仓库根工作区及 Cargo.lock。测试 UI 导出放在 `tests/ui-test-entry.ts`，不扩张应用入口。
