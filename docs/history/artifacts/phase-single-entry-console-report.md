> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../README.md) · [当前文档 / Current documentation](../../README.md)

# Phase report: single-entry Console host

## Boundary

`nexus-launcher` is the host/runtime boundary. Its `console` mode (and its
no-argument invocation) starts or reconnects to the loopback Agent, starts a
configured Harness, serves the replaceable `nexus-console` view on port 3091,
and supervises Agent availability. The view calls `/launcher/*` only for host
controls and `/v1/*` for Agent-owned business operations.

Agent is an independent long-lived process. `nexusctl` is only a short-lived
HTTP client, and Harness is a separate process supervised by Agent. Launcher
does not embed Agent or transfer its business state into the Console process.

Tauri/Electron packaging is intentionally not part of this phase. A future
native shell will be a replaceable client of the launcher API. A dedicated
Harness Web shell, if ever needed, is a separate optional component and is not
a dependency of the launcher or Agent.

## Delivered

- Added the `console` launcher command and made no-argument invocation enter
  the same host mode.
- Added `--console-dir`, `NEXUS_CONSOLE_DIR`, and `--no-open` for development
  and packaging control.
- Added a loopback static host at `127.0.0.1:3091` with `/launcher/status`,
  `/launcher/agent`, and `/launcher/logs` endpoints.
- Added Agent start/stop/restart controls to the Console view. The view
  gracefully falls back when served by the Python static preview and launcher
  routes are absent.
- Kept Agent as an independent operating-system process. The Console host
  supervises it while running but does not implicitly stop it when the host
  exits; stopping Agent is an explicit control action.
- Kept Harness, profile, checkpoint, release, update, configuration, and
  diagnostics behavior in the Agent; the launcher does not duplicate business
  state or modify Harness source.

## Deliberate limitation

The current supervisor can only manage child processes it starts in the
current runtime. Safe adoption of a Harness or Agent started by another
launcher remains a separate phase. It must validate PID, executable path,
start-time, and command fingerprint before attaching; ambiguous processes must
remain detached rather than being killed or claimed.

## Verification record

The following checks passed in the phase worktree:

- `cargo fmt --all -- --check` — exit 0.
- `cargo test --workspace --locked` — exit 0; all workspace tests passed
  (8 Agent, 15 Core, 6 Launcher, 9 Protocol; doc-tests passed).
- `cargo build --workspace --release --locked` — exit 0.
- `node --check apps/nexus-console/app.js` — exit 0.
- `git diff --check` — exit 0.
- Loopback smoke with an isolated external data root — `GET /` returned the
  Console HTML, `/launcher/status` reported Agent healthy, `/v1/health`
  returned `ok`, and `/launcher/agent` restart returned a new healthy PID.
  Ctrl+C stopped only the Console host; the Agent remained healthy until an
  explicit `nexus-launcher stop` request, which then returned exit 0.

No Harness source or user `.dsh` data is modified by this phase. A full
workspace Clippy run with `-D warnings` remains outside this phase's scope and
is blocked by pre-existing warnings in `nexus-protocol`/`nexus-core`.
