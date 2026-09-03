# Phase report: console-port

- Task: `nexus-console`
- Phase: `console-port`
- Status: PASS (implementation complete; commit recorded by worktree flow)
- Scope: propagate the configured Launcher Console port to Agent loopback CORS
- Changed paths:
  - `crates/nexus-agent/src/lib.rs`
  - `crates/nexus-launcher/src/main.rs`
  - `apps/nexus-console/README.md`
  - `docs/architecture-baseline.md`

## Outcome

The Agent now permits only `http://127.0.0.1:<configured-port>`,
`http://localhost:<configured-port>`, and `http://[::1]:<configured-port>` as
Console origins. The launcher sets `NEXUS_CONSOLE_PORT` before spawning a new
Agent, keeping `launcher.json`/CLI `console_port` changes usable end to end.
Malformed, remote, HTTPS, credential-bearing, path-bearing, and wrong-port
origins remain forbidden. A directly started Agent keeps the safe 3091 default
unless its environment supplies the same explicit port.

## Verification

- `cargo test --workspace --locked` — PASS (all workspace tests)
- `cargo build --workspace --release --locked` — PASS
- `cargo fmt --all -- --check` — PASS
- `node --check apps/nexus-console/app.js` — PASS
- `git diff --check` — PASS
- Isolated custom-port smoke — PASS:
  - Launcher served Console HTTP 200 on `127.0.0.1:3191`.
  - Agent preflight returned 204 and `Access-Control-Allow-Origin:
    http://127.0.0.1:3191`.
  - A remote origin received HTTP 403.
  - Explicit cleanup left no smoke Agent/Launcher listener.

The strict `-D warnings` Clippy gate remains blocked by pre-existing warnings
outside this phase (manual defaults/large enum and similar baseline findings).

## Not done in this phase

Native Tauri/Electron packaging, Harness source changes, and remote binding
remain out of scope.
