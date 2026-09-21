# 下一版启动与恢复改进：本地验收 / Local verification

日期 / Date: 2026-09-21

本记录对应 `0ffc7c5` 之后的本地修改，不代表已经发布。验证环境为 Windows x64；没有将结果扩大为 macOS、Linux 或所有第三方插件的兼容承诺。

This record covers local changes after `0ffc7c5`, not a published release. Validation ran on Windows x64. Results do not imply macOS, Linux or universal third-party plugin compatibility.

## 功能与证据 / Behavior and evidence

| 范围 / Area | 实现及验证 / Implementation and verification |
| --- | --- |
| 启动诊断 / Startup diagnosis | Web 和 Desktop 共用 `startup-diagnosis.mjs`，保留官方错误，区分缺失依赖、配置、接口不兼容及服务等待。诊断签名测试、Desktop 界面点击测试及兼容性回归覆盖分类和导航；不会根据等待服务名称推断责任插件。 / Shared classification retains original errors and evidence; signature, UI and compatibility tests cover remedies without attributing fault from waiting service names. |
| 离线依赖修复 / Offline dependency repair | 按锁文件及本地包身份生成预览，确认指纹后仅补缺失链接；先保存计划、锁文件、相关清单及 pnpm 布局元数据，再逐项记录结果并复检。已有文件和损坏链接保持不变。过期预览、路径越界、版本不符和 pnpm 11 缩短目录名均有测试。真实 Node 子进程验证修复前模块缺失、修复后成功导入。 / A fingerprint-bound preview restores only missing links from exact local packages, records metadata before changes, journals results and rechecks links. Existing entries remain untouched. Tests cover stale plans, containment, identity and shortened pnpm paths; a real Node fixture imports successfully after repair. |
| 操作一致性 / Lifecycle consistency | Web 托盘操作复用工作台动作；Desktop 重启共用后台操作，停止失败不再启动，后续停止取消待执行重启。托盘取消绑定原启动 ID，旧菜单不会取消新任务。后台真实子进程取消测试证明检查进程退出、未启动 Harness；测试清理等待后台回收。 / Shared actions enforce stop-before-restart and operation-bound cancellation. Real child-process cancellation checks verify exit and no Harness spawn. |
| 实际配置 / Effective profile | Desktop 错误指引明确 `desktop` 配置档，工作台测试覆盖模式互斥、配置入口及不支持 Desktop 时隐藏选项。 / Desktop guidance identifies its actual profile; workbench tests cover mode locking, profile access and capability gating. |
| 启动计时及优化 / Timings and optimization | Web 按操作记录检查、兼容检查和创建进程耗时；完成后冻结，新操作重新计时。Desktop 记录准备阶段耗时。运行时缓存逐项检查改为最多 16 个并行文件系统请求，保留元数据、链接范围检查及损坏重建。 / Per-operation timings exclude subsequent use. Bounded parallel cache checks preserve verification and repair behavior. |
| 更新流程 / Updates | 新版本旁提供发布页入口。隔离 Windows NSIS 验收实际点击确认下载、观察进度、中断下载后恢复、稍后重启、确认安装；重启后核对版本及安装文件摘要。测试安装已卸载。发布页按钮另有组件点击测试。 / Isolated NSIS acceptance exercised consent, progress, interruption recovery, deferred restart and installation, then verified the relaunched version and installed hash. The notes action has a component click test. |

## 测量与验证层次 / Measurements and verification tiers

- 真实 Harness `0.1.6-alpha.2` 的临时配置档兼容性检查通过，约 24.7 秒；测试断言原配置未被改写。此项不是重启操作系统后的完整冷启动计时。
- Real Harness `0.1.6-alpha.2` passed an isolated temporary-profile compatibility check in approximately 24.7 seconds, with preservation assertions. This is not an operating-system cold-cache startup measurement.
- 同一 8,909 项运行时缓存，串行检查约 1.5–1.8 秒，有限并行约 0.5–0.7 秒；输出与原缓存记录完全一致。不能把这一阶段的加速等同于完整 Web/Desktop 启动加速。
- On the same 8,909-entry runtime cache, serial checks took approximately 1.5–1.8 seconds and bounded parallel checks 0.5–0.7 seconds, producing the same inventory. This does not establish total Web/Desktop startup improvement.
- 前端完整回归 139 项通过；Electron 回归 72 项通过、3 项跳过。跳过项目不计入通过，涉及条件性上游集成和非 Windows 进程行为。
- Full frontend regression: 139 passed. Electron regression: 72 passed, 3 skipped. Skips cover conditional upstream integrations and non-Windows process behavior and are not counted as passes.
- Rust 工作区回归通过；默认忽略的项目不计入通过。额外显式执行的真实 Node 修复、真实安装目录只读预览和检查子进程取消均通过。取消测试已修正两处测试假设：不存在的工作目录也代表清理完毕，删除测试父目录前需等待后台回收。
- Rust workspace regression passed; ignored cases are excluded. Explicit real-Node repair, installed-release preview and owned-process cancellation checks also passed. The cancellation test now accepts an absent scratch directory as clean and waits for background reclamation before fixture deletion.

本地原始证据 / Local evidence:

- `.tmp-ui-audit/goal-ui-final.log`
- `.tmp-ui-audit/goal-electron-final.log`
- `.tmp-ui-audit/goal-workspace-final.log`
- `.tmp-ui-audit/shared-diagnosis-owned-probe.log`
- `.tmp-ui-audit/shared-diagnosis-compatibility.log`
- `.tmp-ui-audit/dependency-installed-preview.log`
- `.tmp-ui-audit/dependency-repair-boundaries.log`
- `.tmp-ui-audit/dependency-module-load.log`
- `.tmp-ui-audit/inventory-comparison.log`
- `.tmp-ui-audit/desktop-preparation-parallel.log`
- `.tmp-ui-audit/goal-real-harness-check.log`
- `apps/nexus-launcher/electron-dist/update-e2e-wQoFCe/report.json`

原始测试产物留在本机，不随源码发布。 / Raw artifacts remain local and are not distributed with source.

## 边界 / Boundaries

修复不会联网下载、替换已有损坏链接或承诺修好所有依赖。模块导入恢复不等于完整 Harness 就绪，界面仍要求启动受影响模式并依据启动检查结果判断。没有代替上游修补 dshmarket 的 Desktop 包管理，也没有扩张平台支持范围或承诺捕获全部运行时错误。发布、跨平台 CI 及发行包验收另行执行。

Repair does not download packages, replace existing broken links or claim to fix every dependency. Successful module import is not full Harness readiness; the affected mode still requires its startup check. No workaround replaces upstream dshmarket Desktop package management, expands platform support or promises to capture all runtime failures. Publication, cross-platform CI and release-package acceptance remain separate work.

## 0.1.10 发布补充 / Release addendum

浏览器就绪门禁修复后，前端 139 项通过，客户端审计与托盘专项 14 项通过；D 盘成品资源与实际 Electron/Agent 启动检查通过。工作台约 2.7 秒就绪，不是 Harness 冷启动时间。Windows 原生气泡的实际弹出和点击已由用户确认验收通过；macOS/Linux 的通知仍须分别验收。

After browser readiness gating, 139 frontend checks and 14 client-audit/tray checks passed. The D-drive packaged resources and actual Electron/Agent launch passed. Workbench readiness was about 2.7 seconds, not Harness cold-start time. The user confirmed actual Windows balloon display and click acceptance; macOS/Linux notifications require their own device acceptance.

发布检查发现 Windows 状态原子替换可能遇到短暂共享冲突，已增加最多约 0.5 秒的有限重试，保持旧状态且不删除目标文件。模拟暂时/永久错误、真实停止流程及可见 Desktop 窗口专项 4 项通过。本地终端测试需与 CI 一样设置完整 `NEXUS_TEST_POWERSHELL` 路径；未设置时的失败单独保留，不计为成功。

Release checks exposed transient Windows sharing conflicts during atomic state replacement. Bounded retries now retain the prior state without deleting the destination, for up to about 0.5 seconds. Four targeted checks passed, covering transient/permanent errors, actual stop behavior and a visible Desktop window. Local terminal tests require an absolute `NEXUS_TEST_POWERSHELL` path, as CI already provides; the earlier unset-path failure is retained separately and not counted as success.
