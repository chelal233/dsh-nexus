# Phase 8 report: Nexus-owned configuration management

## Worktree and scope

- Task: `nexus-bootstrap`
- Phase: `config-management`
- Worktree: `E:\git\dsh-nexus-config-config`
- Scope: typed v1 configuration protocol, atomic Nexus config store, Agent
  config API, CLI config commands, credential-safe response redaction, tests,
  and documentation.
- Exclusions: no Harness source/package/lockfile/build changes, no `.dsh`
  traversal, no production data, deployment, or native UI build.

## Delivered

- Added `HarnessConfigPayload`, `UpdateConfigPayload`, and explicit
  `ConfigAction`/`ConfigCommand`/`ConfigResponse` wire types.
- Added `ConfigStore` for validated, same-directory atomic writes to the
  Nexus-owned `config.json`; missing configuration remains a valid empty
  document.
- Added `GET|POST /v1/config` with status, set, and clear operations. Harness
  changes are rejected while the supervised child is starting/running; update
  changes are rejected while an update job is running.
- Added `nexusctl config status`, `config clear-harness`, and
  `config clear-update`; JSON output is available for GUI/WebShell clients.
- Responses preserve useful flag names while redacting sensitive argument
  values, including `--api-key value` and `--token=value` forms.
- Added round-trip/atomicity coverage and a release-binary API/CLI smoke script
  at `E:\git\dsh-nexus-controller-artifacts\config-management\config-smoke.ps1`.

## Verification

- `cargo fmt --all` — pass.
- `cargo test --workspace --locked` — 30 tests passed, 0 failed.
- `cargo build --workspace --release --locked` — pass.
- `git diff --check` — pass.
- Release-binary config smoke — pass: Agent health, empty initial config,
  redacted Harness secret, durable Harness/update writes, `nexusctl config
  status --json`, clears, and graceful Agent shutdown.

## Boundaries carried forward

`config.json` is Nexus-owned and does not replace Harness' `$HOME/.dsh` data.
The API validates and coordinates; it does not infer Harness flags, edit the
upstream tree, or silently restart processes. Tauri/Electron remains a
replaceable client of the loopback v1 protocol. A native UI and resident
launcher are the next implementation slice; their absence does not reduce the
headless Agent's functionality.
