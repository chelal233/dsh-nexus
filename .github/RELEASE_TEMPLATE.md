# Nexus 1.0.3

## 1.0.3 — 2026-09-26

- 修复 macOS/Linux 内置 Node 的 bin/npm、bin/npx 及存在的 Corepack 入口：打包展开符号链接后仍从正确的包目录启动，避免选择 bin/node 时出现 npm_probe_failed 并连带显示 pnpm 不可用。旧构建缓存会重新生成；已安装包需要升级才能获得修复。
- 官方桌面端后台运行时可再次“打开桌面端”，恢复已有窗口而不重启任务。按实际宿主能力显示入口，旧独立宿主保留原有控制方式。
- 明确关闭窗口后进程可能继续运行；切换模式或修改数据前仍须中止桌面端。保留新旧 Harness 接口兼容，不自动升级用户 Harness。

### 验证范围

平台构建和自动化检查不等于所有设备上的真实升级验收。内置 Harness 仍为 0.1.6-alpha.2；新版兼容适配不代表所有会话迁移及第三方插件已验收。

---

## 1.0.3 — 2026-09-26

- Fix bundled macOS/Linux Node bin/npm, bin/npx and available Corepack entry points. They now launch from the correct package directory after packaging dereferences symbolic links, preventing npm_probe_failed and the resulting unavailable pnpm status when bin/node is selected. Old build caches are regenerated; installed applications need an update to receive the fix.
- Open Desktop can restore an existing background desktop window without restarting its tasks. The running host advertises support; older independent hosts retain their existing controls.
- Clarify that closing a window may leave Desktop running. Stop Desktop before switching modes or changing data. Compatibility with older and newer Harness interfaces remains; user Harness installations are not upgraded automatically.

### Verification scope

Platform builds and automated checks do not establish real upgrade acceptance on every device. Bundled Harness remains 0.1.6-alpha.2; compatibility work does not certify every session migration or third-party plugin.
