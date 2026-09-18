> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../README.md) · [当前文档 / Current documentation](../../README.md)

# Phase report: launcher-config

- Task: `nexus-console`
- Phase: `launcher-config`
- Status: PASS (implementation complete; commit recorded by worktree flow)
- Scope: launcher configuration precedence and Launcher-owned Harness UI/token access
- Changed paths:
  - `crates/nexus-launcher/src/main.rs`
  - `apps/nexus-console/index.html`
  - `apps/nexus-console/app.js`
  - `apps/nexus-console/README.md`
  - `docs/architecture-baseline.md`

## Outcome

`nexus-launcher` now loads an optional `<data-root>/launcher.json` with
`CLI > launcher.json > environment > defaults` precedence. `--data-dir` is
resolved first and remains outside that file. Invalid file values fail closed;
unknown fields are ignored for forward compatibility.

The Launcher Console host now exposes `GET /launcher/harness` and
`POST /launcher/harness` with `{"action":"open"}`. It reads only a 64 KiB
tail of Nexus-owned Harness stdout/stderr logs, accepts only plain HTTP
loopback URLs, prefers token-bearing URLs, and exposes the newest URL/token to
the Console. The Console adds open, refresh, and copy-token actions. No
Harness source or `.dsh` data is read or modified.

## Verification

- `cargo test --workspace --locked` — PASS (all workspace tests)
- `cargo build --workspace --release --locked` — PASS
- `cargo fmt --all -- --check` — PASS
- `node --check apps/nexus-console/app.js` — PASS
- `git diff --check` — PASS
- Release smoke with an isolated runtime directory — PASS:
  - Launcher served Console HTTP 200 on `127.0.0.1:3091`.
  - `/launcher/status` reported a healthy independent Agent.
  - A synthetic loopback Harness auth line was discovered as the expected URL
    and token by `/launcher/harness`.
  - Explicit Agent stop completed and no smoke Agent/Launcher listener remained.

`cargo clippy --workspace --locked -- -D warnings` remains blocked by the
pre-existing manual-default warnings in `nexus-protocol`; no unrelated source
was changed for that baseline issue.

## Not done in this phase

Native Tauri/Electron packaging, Harness source changes, `.dsh` migration,
remote URLs, and a new Nexus authentication protocol remain out of scope.
