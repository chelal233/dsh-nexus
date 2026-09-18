> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../../README.md) · [当前文档 / Current documentation](../../../README.md)

# P0-4a runtime discovery / runtime-budget fix

状态：runtime-budget 修复位于工作树 `E:\git\dsh-nexus-phases\runtime-budget`、分支
`codex/nexus-takeover/runtime-budget`，固定基线
`69d508493102c1460adb679f34aa89c9acebb630`；只改 Agent runtime 发现实现、此报告和项目状态，等待主控独立复核，不合并、不部署。

## 本轮失败证据

原实现先在异步 task 上同步执行 `ProbeConfig::from_paths`、最多 256 个 PATH 目录的候选 canonicalize/metadata，以及 portable `read_dir`/metadata/canonicalize；这些操作发生在 `timeout_at` 之外。UNC、映射网络盘或异常文件系统可卡住 executor 线程，使声明的 6 秒轮次预算失真。子进程 deadline 又按 `min(round_deadline, now + child)` 计算，超时后另加 1 秒 cleanup，晚候选最坏可超过 round deadline。

测试先加入受控枚举阻塞点；修复前运行
`cargo test --offline -p nexus-agent slow_system_enumeration_cannot_block_the_round_response -- --nocapture`
退出 101：75ms round 未返回，200ms 防挂 timeout 触发，直接证明同步枚举绕过预算。

## 本轮修复与真实保证

- `GET /v1/runtime` 从配置发现开始使用同一个绝对 6 秒 deadline；配置、system/portable 枚举、shim/cwd 文件检查、每个候选准备和等待 blocking 许可都消耗这一个预算。三项工具仍并发观察，因此整个响应受 6 秒轮次约束，并留在共享 AgentClient 8 秒 HTTP timeout 内。
- 同步文件系统调用由全局 3 许可 blocking owner 承担，先取许可再 `spawn_blocking`。Rust 无法强制终止一个已卡死的同步文件系统线程；超时任务会继续持有许可直到它真实返回，但永久挂起/排队的 blocking task 数最多为 3，后续请求只等待剩余 deadline，不会无限追加挂起线程或工作项。
- Windows PATH、数据根、portable 根、probe cwd 和 canonical 结果拒绝 UNC；盘符路径先用只读 `GetDriveTypeW` 拒绝映射网络盘。相对 PATH 条目也不再进入候选。portable 的既有 reparse/canonical containment 继续生效。
- 子进程执行 deadline 现在是 `min(round_deadline - cleanup, now + child)`；仅在还有 cleanup 预算时启动，kill + wait 使用 round deadline 的剩余时间。调用方取消后，持有 child 的 detached bounded task 仍完成 kill/wait；`kill_on_drop` 继续作为 deadline 到期时的最后终止保护。
- 零预算无法安全执行文件系统验证，因此保留 `probe_budget_exceeded`，path 上下文改为 PATH 顺序中的首个词法候选；正常预算下的 canonical 绝对 path/source/version、system 优先、Corepack fail-closed、输出上限和 portable containment 行为不变。

## 既有实现（本轮保持）

- `nexus-agent` 的只读 `GET /v1/runtime` 按 `git`、`node`、`pnpm` 顺序返回协议 `v1` 工具列表。有效结果包含 canonical 绝对路径、`system`/`nexus` 来源和经过工具格式校验的版本文本；缺失或不安全候选返回 `available=false`，并保留稳定 `reason`、`source`、`path` 上下文。
- PATH 按目录优先、受限 `PATHEXT` 顺序（Windows `exe`、`cmd`、`bat`）逐个尝试；system 有效结果优先于 Nexus portable。git 保持 PATH-only，node/pnpm 只检查数据根 `runtimes/` 的固定浅层候选。PATH 条目、system 候选、portable runtime 条目和 portable 候选均有硬上限。
- Windows `.cmd`/`.bat` 只在安全路径上经绝对系统 `%SystemRoot%\System32\cmd.exe /d /s /c call` 探测；路径含 `&|<>^()%!`、引号或控制字符时 fail-closed，扩展名外的伪 cmd 不进入候选。探测进程设置 `CREATE_NO_WINDOW`。
- Corepack shim 不执行：跨平台检查 canonical target、native image magic、脚本扩展、shebang 和脚本文本；Corepack 标记返回 `corepack_shim_unverified`，无法证明的脚本/文件返回 `shim_unverified`。探测环境显式关闭 Corepack network、download prompt、default latest、auto pin、project spec，并把 npm userconfig 指向空设备。
- 探测 cwd 按既有无写入候选回退（TEMP、Nexus run/root、当前目录、当前可执行文件目录），逐个拒绝祖先可见的 `package.json` 与 pnpm workspace manifest。不会写入配置、PATH、Corepack 缓存或用户目录。
- 整个 runtime 观察请求有 6 秒绝对轮次预算（小于共享 AgentClient 的 8 秒 HTTP timeout）、单子进程最多 5 秒且预留 1 秒 bounded kill/wait 清理；输出最多 16 KiB。超时/超限/非零/空 stdout/畸形版本均正常 unavailable。子进程探测在独立 bounded task 中持有，调用方取消不会跳过受限清理。
- 数据根自身与 `runtimes/` 根的 reparse point 直接拒绝；portable candidate canonical 后仍须位于 canonical `runtimes/` 根内，阻止 junction/symlink 逃逸。
- 共享 `nexus-launcher-core` 补齐 `/v1/runtime` 与既有 `/v1/releases/tags` 的 Agent allowlist，二者均只接受无 body GET，POST 和非空 GET body 拒绝。Tauri 移除 runtime 重复 validator，统一复用共享 method/body gate；原有 Tauri route 白名单和 Agent `/v1/releases/tags` 原生网络逻辑不变。

## 测试与检查

| 命令 | 退出码 | 结果 |
| --- | ---: | --- |
| `cargo test --offline -p nexus-agent runtime::tests --no-fail-fast` | 0 | 26/26 runtime tests 通过；新增慢配置/枚举、UNC 预拒绝、晚候选阻塞、取消清理、blocking owner 硬上限的确定性覆盖 |
| `cargo test --offline -p nexus-agent --no-fail-fast` | 0 | Agent 88/88 tests 及 doc-tests 通过 |
| `rustfmt --edition 2021 --check crates/nexus-agent/src/runtime.rs` | 0 | runtime.rs 格式通过 |
| `git diff --check` | 0 | 本轮 diff 无空白错误 |

此前报告中的 `empty_stdout_is_unavailable` 退出 1 是初稿 fixture helper 尚未接线造成的编译 scaffolding 失败，不是行为 RED；现在已有真实空 stdout、非零、超时、超限 fixture，最终行为测试通过。`cargo fmt --all -- --check` 未作为验收依据：它仍报告基线中本轮范围外的 `nexus-agent/src/lib.rs`、`supervisor.rs`、`updater.rs`、`nexus-core` 等格式差异。

当前主机只有 `x86_64-pc-windows-msvc` target；Unix Corepack script 不执行和 portable symlink escape 条件测试代码存在，但本机未编译/执行 Unix 分支；Windows junction escape fixture、真实映射盘与网络文件系统阻塞也未实跑。测试通过受控 hook 证明 deadline 与 blocking 所有权；它不宣称 Rust 能杀死已阻塞的同步线程。未启动真实 Harness，未安装/下载，未改系统 PATH 或用户配置。

## 交接边界

本阶段只宣称 P0-4a runtime discovery 的请求预算修复；既有 P0-1 shared Agent route allowlist 行为保持。P0-4b/c 的安装、下载、显式 Corepack provisioning、运行时注入和 UI 仍待后续阶段。

官方 Corepack README 说明只有 Node `>=14.19` 且 `<25` 随 Node 分发 Corepack，且项目/known-good 场景可能下载并缓存 package manager；不能把所有 Node 版本视为自带 Corepack，也不能把 pnpm shim 调用视为天然无下载：<https://github.com/nodejs/corepack>。
