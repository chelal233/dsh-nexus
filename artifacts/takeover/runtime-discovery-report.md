# P0-4a runtime discovery

状态：本报告只覆盖 P0-4a，提交前交由主控人工审阅。工作树为
`E:\git\dsh-nexus-phases\runtime-discovery`，分支为
`codex/nexus-takeover/runtime-discovery`，基线为 `df3cff0`。

## 实现

- `nexus-agent` 新增只读 `GET /v1/runtime`，按 `git`、`node`、`pnpm` 顺序返回协议版本 `v1` 的工具列表。每个有效结果包含实际 canonical 绝对路径、`system`/`nexus` 来源和经过格式校验的版本文本。
- PATH 候选先逐个探测，只有有效版本才赢得 system 优先级；node/pnpm 随后才检查数据根下固定浅层的 `runtimes/` 候选。git 保持 PATH-only。
- 缺失、非零、空/畸形 stdout、超时、输出超限和不安全候选均正常返回 unavailable；若存在被拒绝候选，`reason`、`source`、`path` 标识稳定原因和发现位置，纯缺失使用 `not_found`。
- Windows `.cmd`/`.bat` 仅在安全路径上通过 `cmd /d /s /c call` 探测；路径含 `&|<>^()%!` 等 shell 元字符显式拒绝。Corepack shim 不执行，返回 `corepack_shim_unverified`，不把 Corepack 版本误报为 pnpm。
- 探测 cwd 拒绝向上可见的 `package.json`/pnpm workspace manifest，并显式设置 Corepack network、download prompt、default latest、auto pin、project spec 为关闭；npm userconfig 指向空设备。所有子进程使用输出上限、5 秒超时、`start_kill` 加 1 秒有界 wait 清理，Windows 设置 `CREATE_NO_WINDOW`。
- portable 根自身以及数据根自身的 symlink/junction/reparse point 拒绝；候选 canonical 后再次要求位于 canonical `runtimes` 根内，避免 junction 逃逸。
- Tauri 白名单加入 `/v1/runtime`，桥接前只允许无 body 的 GET；Agent 路由本身只注册 GET。

## 测试与检查

| 命令 | 退出码 | 结果 |
| --- | ---: | --- |
| `cargo test --offline -p nexus-agent empty_stdout_is_unavailable`（最小 failing test，修复前） | 1 | 预期失败：fixture helper 尚未实现，先建立失败证据 |
| `cargo test --offline -p nexus-agent runtime::tests --no-fail-fast` | 0 | runtime fixture 通过，含缺失、来源优先、portable、版本校验、Corepack 环境、Windows shim 安全回归 |
| `cargo test --offline -p nexus-agent -p nexus-protocol` | 0 | Agent 73 tests、Protocol 11 tests、doc-tests 通过 |
| `rustfmt --edition 2021 --check crates/nexus-agent/src/runtime.rs crates/nexus-protocol/src/lib.rs apps/nexus-launcher/src-tauri/src/main.rs` | 0 | 本阶段 Rust 文件格式检查通过 |
| `cargo test --offline --manifest-path apps/nexus-launcher/src-tauri/Cargo.toml runtime_route_is_read_only_get --no-fail-fast` | 1 | `tauri-build` 在现有工作树找不到 `src-tauri/resources/nexus-agent*` 即停止；未执行会写出范围外资源的 `prepare-agent` |

测试只创建并清理临时 fixture，没有安装、下载、修改 PATH/user config 或启动真实 Harness。完整 workspace formatter 未作为验收依据；既有其他模块存在不相关格式差异。

## 交接边界

当前 Tauri 文件内的 GET-only gate 和 route 白名单已完成，但共享
`crates/nexus-launcher-core/src/lib.rs` 的 `AGENT_ROUTES` 尚未包含
`/v1/runtime`；`AgentClient::request_raw` 会在 Tauri gate 之后再次校验并拒绝
该路径。该文件不在本阶段精确写入范围内，应由主控授权的桥接/UI阶段以最小白名单变更接线，或明确保留为后续集成项。

本阶段不实现 P0-4b/c 的安装、下载、Corepack 显式 provisioning、运行时注入或 UI。
官方 Corepack README 说明只有 Node `>=14.19.0` 且 `<25.0.0` 随 Node 分发 Corepack，且项目/known-good 场景可能下载并缓存 package manager；因此不能把所有 Node 版本视为自带 Corepack，也不能把 pnpm shim 调用视为天然无下载：
<https://github.com/nodejs/corepack>。
