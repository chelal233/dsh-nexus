> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../README.md) · [当前文档 / Current documentation](../../README.md)

# Phase 9 report: launcher bootstrap and replaceable WebShell foundation

## Worktree and scope

- Task: `nexus-bootstrap`
- Phase: `launcher-webshell`
- Worktree: `<WORKSPACE>/dsh-nexus-launcher`
- Branch: `codex/nexus-bootstrap/launcher-webshell`
- Scope: Rust launcher lifecycle, Windows-safe detached process creation,
  foreground supervision, run metadata, static Console/WebShell assets, and
  architecture documentation.
- Exclusions: no Harness source/package/lockfile/build changes, no `.dsh`
  traversal or migration, no production data, deployment, native Tauri/Electron
  packaging, or browser-origin CORS changes.

## Delivered

- Added `nexus-launcher` with `start`, `run`/`foreground`, `stop`, `status`,
  and `logs` commands. It resolves the sibling Agent or an explicit
  `--agent`/`NEXUS_AGENT_BIN`, owns only `run/agent.lock` and `run/agent.json`,
  and never scans for or kills arbitrary processes.
- Added Windows detached startup using an explicit inheritable-handle list so
  PowerShell capture pipes cannot keep the Agent or launcher alive. Agent
  stdout/stderr are appended under the Nexus-owned `logs/` directory.
- Added foreground supervision with Ctrl+C and loopback shutdown handling,
  bounded child reaping, and exact-PID termination only as a timeout fallback.
  Health probing uses a longer connection window to tolerate a concurrently
  polling local WebShell during Agent startup.
- Added `apps/nexus-console/`, a dependency-free static WebShell that validates
  loopback API targets, renders Agent/Harness/profile/release/update/checkpoint/
  diagnostics/config state, and sends explicit v1 actions. It owns no runtime
  state and is loadable by a future Tauri/Electron shell or same-origin proxy.
- Updated the architecture baseline to make the launcher and replaceable
  WebShell boundaries explicit; Harness remains an immutable upstream runtime.

## Verification

- `cargo fmt --all -- --check` — pass.
- `cargo test --workspace --locked` — 33 tests passed, 0 failed.
- `cargo build --workspace --release --locked` — pass.
- `git diff --check` — pass.
- Release-binary lifecycle/WebShell smoke — pass via
  `<WORKSPACE>/dsh-nexus-controller-artifacts\launcher-webshell\launcher-smoke.ps1`:
  detached start, idempotent start, status/log paths, stale-lock recovery,
  graceful/idempotent stop, foreground run under Windows PowerShell with
  redirected output, exact cleanup, and static asset/reference checks.
- Additional foreground probes with concurrent early loopback polling passed
  after the bounded connection-window fix.

## Boundaries carried forward

The launcher is a process boundary, not a second control plane. Agent APIs and
`nexusctl` remain usable without any GUI, and the launcher does not own Harness
profiles, releases, checkpoints, or `$HOME/.dsh`. Native Tauri/Electron
packaging, browser-origin CORS/same-origin integration, and resident tray UI
remain separate follow-up slices; their absence does not reduce headless Agent
functionality.
