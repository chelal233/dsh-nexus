# Phase 3 report: profiles and external checkpoints

## Worktree and scope

- Task: `nexus-bootstrap`
- Phase: `profile-checkpoint`
- Worktree: `E:\git\dsh-nexus-profile-checkpoint`
- Branch: `codex/nexus-bootstrap/profile-checkpoint`
- Base: `3142ca5b3f8db9490565ba3572d176c05a413d1c`
- Scope: the Rust workspace, `docs/architecture-baseline.md`, and this
  report only.
- Exclusions: no Harness/Desktop/Tauri source, no `Cargo.lock` changes, no
  writes under `D:\dsh-local`, no deployment, and no real Harness launch.

## Delivered

- Added stable v1 JSON types for profile list/status/select and checkpoint
  list/create/restore operations. Profile names and checkpoint IDs are
  bounded safe ASCII identifiers; manifests and requests round-trip in tests.
- Added Nexus-owned `ProfileStore` and `ProfileCatalog`. The default active
  profile is `web`; selection validates names, records known profiles, and
  atomically publishes `profiles.json` below `NEXUS_DATA_DIR` (or the normal
  Nexus data root). No Harness data directory is consulted.
- Added `CheckpointStore`. It atomically writes `checkpoints/cp-*.json`
  manifests containing timestamp, profile, release, note, and a Nexus state
  summary. List/read/restore validate IDs and manifest/profile consistency;
  restore is metadata-only and never copies or rewrites `.dsh` data.
- Agent boot loads the active profile and keeps `AgentState.profile` and
  `state.json` metadata aligned. `GET|POST /v1/profiles` supports list/status/
  select; switching while Harness is starting/running returns HTTP 409 without
  an implicit restart. `GET|POST /v1/checkpoints` supports list/create/restore;
  restore rejects a running Harness, applies profile/release metadata while
  stopped, and never auto-starts Harness.
- Harness launch supports an explicit `{profile}` replacement in
  `HarnessLaunchSpec.args`; `start_with_profile` and `restart_with_profile`
  accept the selected profile while `start`/`restart` remain compatible
  wrappers. No Harness CLI semantics, `DSH_HOME`, or Harness configuration is
  inferred or injected.
- Extended `nexusctl` with `profile status|list|select NAME` and
  `checkpoint list|create [--note TEXT]|restore ID`, retaining existing
  `status` and `harness` commands plus `--json` and readable HTTP errors.
- Updated the architecture baseline to document Nexus-owned profile rendering,
  metadata-only checkpoints, and the remaining update/release/auth/Tauri
  boundaries.

## Test-first and verification

The new protocol test was first run before the types existed and failed to
compile; it passed after implementation. A supervisor placeholder test was
also run before the rendering method existed and failed to compile, then
passed after implementation. Final commands were run from this worktree with
Rust 1.98.0:

- `cargo fmt --all -- --check` — exit 0.
- `cargo check --workspace --locked` — exit 0.
- `cargo test --workspace --locked` — exit 0; 18 unit tests passed, 0 failed.
- `git diff --check` — exit 0.
- Fake Nexus store tests covered default/selected profiles and a checkpoint
  manifest without launching a real Harness. No external `.dsh` data was read
  or written.

## Not done / boundaries

- Nexus does not discover or mutate Harness source, credentials, sessions, or
  its data directory. HTTPS readiness, non-loopback readiness, and remote
  Agent binding are intentionally unsupported.
- Checkpoint restore applies only Nexus profile/release metadata; it does not
  restore Harness sessions, credentials, user files, or `.dsh` state.
- Profile names are metadata and are rendered only where an explicit
  `{profile}` placeholder is configured. Nexus does not guess a Harness CLI
  flag or internal implementation.
- Update/release promotion, authentication, Tauri/Desktop integration, and
  remote control remain future phases.
