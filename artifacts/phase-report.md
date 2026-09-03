# Phase 7 report: bounded diagnostics collection

## Worktree and scope

- Task: `nexus-bootstrap`
- Phase: `diagnostics`
- Worktree: `E:\git\dsh-nexus-diagnostics`
- Branch: `codex/nexus-bootstrap/diagnostics`
- Base: `bb9df16f1cf6d9b99c857f3292ccdd622b51d364`
- Scope: v1 diagnostics protocol, Nexus core store, Agent/CLI routes, docs,
  and this report.
- Exclusions: no Harness source or `.dsh` traversal, no production data,
  deployment, or UI shell.

## Delivered

- Added `DiagnosticsStore` with a Nexus-only allowlist: runtime state,
  profiles, release pointers, update state, and text logs. It never reads the
  process environment, Harness working/data directories, or `$HOME/.dsh`.
- Added bounded collection (`64` files; metadata capped at `512 KiB`, logs at
  `256 KiB`) with binary omission, truncation markers, safe filenames, and
  create-new/sync writes into `diagnostics/<id>/files/`.
- Added conservative line redaction for credential-shaped log fields
  (`password`, token, authorization, cookie, private key, and related markers)
  before a bundle is published. The response exposes local path and metadata,
  not raw file contents.
- Added durable bundle manifests and `GET|POST /v1/diagnostics`; extended
  `nexusctl diagnostics status|collect` with optional notes and JSON output.

## Verification

- `cargo fmt --all -- --check` — pass.
- `cargo test --workspace --locked` — 28 tests passed, 0 failed.
- `cargo build --workspace --locked --release` — pass.
- `git diff --check` — pass.
- Runtime smoke returned `COLLECT_STATUS=201`, created a bundle with three
  allowlisted files, redacted an authorization line, reported
  `SECRET_PRESENT=False`, returned it from status, and shut down the Agent with
  `AGENT_EXITED=True`.

## Boundaries carried forward

Diagnostics are intentionally a local, bounded handoff primitive rather than
a general archive or cloud uploader. Tauri/Electron, authentication, update
auto-policy, plugin activation, and any remote export remain replaceable layers
over the headless Agent protocol.
