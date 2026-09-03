# Phase 1 report: headless Agent core

## Worktree

- Task: `nexus-bootstrap`
- Phase: `core-agent`
- Worktree: `E:\git\dsh-nexus-core-agent`
- Branch: `codex/nexus-bootstrap/core-agent`
- Base: `8cfecef1f6a0d8db0b027efd7821533ab35abb41`
- Scope: Rust workspace, committed `Cargo.lock`, `nexus-protocol`,
  `nexus-core`, `nexus-agent`, `nexus-cli`, architecture baseline,
  `.gitignore`, and this report.
- Harness source paths were not read or modified as part of implementation.

## Delivered

- Added a Rust 2021 workspace with four crates and no Tauri/Electron dependency.
- Added `v1` JSON wire types for health, state, and lifecycle shutdown.
- Added platform-neutral Nexus data-root resolution with `NEXUS_DATA_DIR` and
  `NEXUS_AGENT_PORT` overrides, while keeping the Agent loopback-only.
- Added an in-memory Agent state model and reserved Nexus-owned data paths.
- Added a foreground Agent HTTP server on `127.0.0.1:3090` by default.
- Added `GET /v1/health`, `GET /v1/state`, `POST /v1/lifecycle`, and
  `POST /v1/shutdown`.
- Added `nexusctl status` with human-readable and `--json` output.
- Documented Harness immutability and the replaceable Console/WebShell boundary.

## Verification

All commands were run from the phase worktree with Rust 1.98.0 installed at
`C:\Users\PC\.cargo\bin`.

- `cargo fmt --all -- --check` — exit 0.
- `cargo metadata --no-deps --format-version 1` — exit 0; all four workspace
  members were discovered.
- `cargo check --workspace --locked` — exit 0.
- `cargo test --workspace --locked` — exit 0; 5 unit tests passed, 0 failed.
- Runtime smoke on port `3190` with an isolated data directory — passed:
  `/v1/health` returned `{"api_version":"v1","service":"nexus-agent","status":"ok"}`;
  `/v1/state` returned lifecycle `running` and Harness `detached`;
  `nexusctl status` and `nexusctl status --json` returned the same state;
  `POST /v1/shutdown` returned HTTP 202 and the foreground Agent exited.
- `Cargo.lock` is committed to make this executable workspace's dependency
  resolution reproducible; generated `target/` remains ignored.

## Limitations intentionally left for later phases

- Harness process supervision, release slots, update verification, profiles,
  checkpoints, safe mode, diagnostics, and external plugin handling are not
  implemented yet; the protocol and state model leave their boundary explicit.
- State is currently in memory; no credentials, sessions, or secret values are
  persisted or copied.
- Authentication is deferred while the listener is hard-bound to loopback.
- Tauri Console/WebShell is deliberately not part of this phase.
