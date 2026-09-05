# P0-4a runtime discovery review-fix

状态：本报告只覆盖 P0-4a 及本轮发现的 P0-1 Agent route allowlist 接线缺口，提交后交由主控人工审阅。工作树为
`E:\git\dsh-nexus-phases\runtime-fix1`，分支为
`codex/nexus-takeover/runtime-fix1`，基线为
`fc0c5d1409e0e9476d365b641f1e1520fda470e8`。

## 实现

- `nexus-agent` 的只读 `GET /v1/runtime` 按 `git`、`node`、`pnpm` 顺序返回协议 `v1` 工具列表。有效结果包含 canonical 绝对路径、`system`/`nexus` 来源和经过工具格式校验的版本文本；缺失或不安全候选返回 `available=false`，并保留稳定 `reason`、`source`、`path` 上下文。
- PATH 按目录优先、受限 `PATHEXT` 顺序（Windows `exe`、`cmd`、`bat`）逐个尝试；system 有效结果优先于 Nexus portable。git 保持 PATH-only，node/pnpm 只检查数据根 `runtimes/` 的固定浅层候选。PATH 条目、system 候选、portable runtime 条目和 portable 候选均有硬上限。
- Windows `.cmd`/`.bat` 只在安全路径上经绝对系统 `%SystemRoot%\System32\cmd.exe /d /s /c call` 探测；路径含 `&|<>^()%!`、引号或控制字符时 fail-closed，扩展名外的伪 cmd 不进入候选。探测进程设置 `CREATE_NO_WINDOW`。
- Corepack shim 不执行：跨平台检查 canonical target、native image magic、脚本扩展、shebang 和脚本文本；Corepack 标记返回 `corepack_shim_unverified`，无法证明的脚本/文件返回 `shim_unverified`。探测环境显式关闭 Corepack network、download prompt、default latest、auto pin、project spec，并把 npm userconfig 指向空设备。
- 探测 cwd 按既有无写入候选回退（TEMP、Nexus run/root、当前目录、当前可执行文件目录），逐个拒绝祖先可见的 `package.json` 与 pnpm workspace manifest。不会写入配置、PATH、Corepack 缓存或用户目录。
- 每工具有 6 秒整轮预算（小于共享 AgentClient 的 8 秒 HTTP timeout）、单子进程 5 秒窗口、1 秒 bounded kill/wait 清理；输出最多 16 KiB。超时/超限/非零/空 stdout/畸形版本均正常 unavailable。子进程探测在独立 bounded task 中持有，调用方取消时不会以无界等待遗留子进程。
- 数据根自身与 `runtimes/` 根的 reparse point 直接拒绝；portable candidate canonical 后仍须位于 canonical `runtimes/` 根内，阻止 junction/symlink 逃逸。
- 共享 `nexus-launcher-core` 补齐 `/v1/runtime` 与既有 `/v1/releases/tags` 的 Agent allowlist，二者均只接受无 body GET，POST 和非空 GET body 拒绝。Tauri 移除 runtime 重复 validator，统一复用共享 method/body gate；原有 Tauri route 白名单和 Agent `/v1/releases/tags` 原生网络逻辑不变。

## 测试与检查

| 命令 | 退出码 | 结果 |
| --- | ---: | --- |
| `cargo test --offline -p nexus-agent runtime::tests --no-fail-fast` | 0 | 20 runtime fixture tests 通过；含缺失、system 优先、portable、空 stdout、非零、超限、短超时、reaped、零预算 path/reason、cwd fallback、Corepack/Windows shim 回归 |
| `cargo test --offline -p nexus-agent -p nexus-protocol -p nexus-launcher-core --no-fail-fast` | 0 | Agent 82、launcher-core 11、protocol 11 tests 及 doc-tests 全部通过 |
| `$env:CARGO_NET_OFFLINE='true'; node apps/nexus-launcher/src-tauri/scripts/prepare-agent.mjs` | 0 | 本工作树 release 构建并 staging 三份被忽略 Nexus binary；未下载、安装或启动 Harness |
| `cargo test --offline --manifest-path apps/nexus-launcher/src-tauri/Cargo.toml --no-fail-fast` | 0 | Tauri 7 tests 全部通过，资源加载可用 |
| `cargo test --offline --manifest-path apps/nexus-launcher/src-tauri/Cargo.toml native_routes_are_direct_agent_routes --no-fail-fast` | 0 | Tauri shared route/method/body gate 定向测试通过 |
| `rustfmt --edition 2021 --check crates/nexus-agent/src/runtime.rs crates/nexus-launcher-core/src/lib.rs apps/nexus-launcher/src-tauri/src/main.rs` | 0 | 本轮三份 Rust 文件格式通过 |
| `git diff --check` | 0 | 本轮 diff 无空白错误 |

此前报告中的 `empty_stdout_is_unavailable` 退出 1 是初稿 fixture helper 尚未接线造成的编译 scaffolding 失败，不是行为 RED；现在已有真实空 stdout、非零、超时、超限 fixture，最终行为测试通过。`cargo fmt --all -- --check` 未作为验收依据：它仍报告基线中本轮范围外的 `nexus-agent/src/lib.rs`、`supervisor.rs`、`updater.rs`、`nexus-core` 等格式差异。

当前主机只有 `x86_64-pc-windows-msvc` target；Unix Corepack script 不执行和 portable symlink escape 条件测试代码已添加，但本机未编译/执行 Unix 分支；Windows junction escape fixture 也未实跑，Unix/Windows reparse 行为仍待对应 target/权限下验证。未做真实 Harness、安装、下载、网络验收或系统 PATH/user config 写入。

## 交接边界

本提交只宣称 P0-4a runtime discovery，以及为完成真实桥接而补的 P0-1 shared Agent route allowlist 缺口。P0-4b/c 的安装、下载、显式 Corepack provisioning、运行时注入和 UI 仍待后续阶段。

官方 Corepack README 说明只有 Node `>=14.19` 且 `<25` 随 Node 分发 Corepack，且项目/known-good 场景可能下载并缓存 package manager；不能把所有 Node 版本视为自带 Corepack，也不能把 pnpm shim 调用视为天然无下载：<https://github.com/nodejs/corepack>。
