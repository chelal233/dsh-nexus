# Phase 2 report: external Harness supervisor

## Worktree and scope

- Task: `nexus-bootstrap`
- Phase: `harness-supervisor`
- Worktree: `E:\git\dsh-nexus-harness-supervisor`
- Branch: `codex/nexus-bootstrap/harness-supervisor`
- Base: `329d65df0c30faf6d10e6b3e954b27c7476669c5`
- Scope: the Rust workspace, `docs/architecture-baseline.md`, and this
  report only.
- Exclusions: no Harness/Desktop/Tauri source, external directory, or
  `Cargo.lock` changes; no deployment or real Harness launch.

## Delivered

- Added versioned v1 Harness control/status JSON types:
  `HarnessAction`, `HarnessCommand`, `HarnessRuntimeInfo`, and
  `HarnessResponse`. Runtime snapshots carry state and optional PID,
  exit-code, error, and timestamps. Existing lifecycle/state response types
  remain source- and wire-compatible.
- Added `HarnessLaunchSpec` and Nexus-owned configuration loading. The
  optional spec is read from `NEXUS_DATA_DIR/config.json` under `harness` (a
  direct spec object is accepted as well), with explicit
  `NEXUS_HARNESS_PROGRAM`, `NEXUS_HARNESS_ARGS`,
  `NEXUS_HARNESS_WORKING_DIR`, `NEXUS_HARNESS_READINESS_URL`, and
  `NEXUS_HARNESS_READINESS_TIMEOUT_SECS` overrides.
- Added `NexusRuntimeMetadata` and `RuntimeMetadataStore`. `state.json` is
  flushed and atomically replaced beside the Nexus data root, with an
  in-process write gate. It is separate from the external Harness working or
  data directory; Nexus never reads or writes `$HOME/.dsh` or `DSH_HOME`.
- Added the independent Tokio `HarnessSupervisor`. It redirects stdout and
  stderr to Nexus `logs/harness.stdout.log` and `logs/harness.stderr.log`,
  observes natural child exit, waits a bounded five seconds on stop, and then
  uses Tokio's cross-platform kill fallback. Missing configuration is a clear
  `harness_not_configured` error. Readiness is optional, plain HTTP, and
  restricted to `localhost`, `127.0.0.1`, or `[::1]`.
- Added Agent `GET /v1/harness` and `POST /v1/harness` (`start`, `stop`,
  `restart`; `status` is harmlessly accepted), with readable JSON HTTP errors.
  `GET /v1/state` synchronizes the Harness state. Agent boot does not
  auto-start a Harness, and Agent shutdown cleans up an attached child.
- Extended `nexusctl` with `harness status|start|stop` and optional `--json`;
  existing `nexusctl status` behavior remains available. HTTP failures include
  both a readable message and stable error code.
- Updated the architecture baseline with configuration examples and explicit
  Phase 2 boundaries. Profiles, checkpoints, update/release promotion,
  authentication, and Tauri remain later work.

## Test-first and verification

The first protocol test run intentionally failed to compile because the new
Harness types did not exist yet; after implementation it passed. All later
commands were run from this worktree with Rust 1.98.0:

- `cargo fmt --all -- --check` — exit 0.
- `cargo check --workspace --locked` — exit 0.
- `cargo test --workspace --locked` — exit 0; 13 unit tests passed, 0 failed.
- `git diff --check` — exit 0.
- Isolated HTTP smoke with a fake `cmd.exe` command configured through
  `NEXUS_HARNESS_PROGRAM`/`NEXUS_HARNESS_ARGS` — passed: initial
  `detached`, Harness start `running`, `/v1/state` `running`, CLI
  `harness status --json` returned the v1 snapshot, stop `stopped`, and Agent
  shutdown returned HTTP 202. No real Harness installation was used.

## Not done / boundaries

- Nexus does not discover or mutate Harness source, credentials, sessions, or
  its data directory. HTTPS readiness, non-loopback readiness, and remote
  Agent binding are intentionally unsupported.
- There is no automatic restart policy, process-tree/job-object containment,
  profile/checkpoint/update/auth implementation, GUI integration, or release
  promotion in this phase.
