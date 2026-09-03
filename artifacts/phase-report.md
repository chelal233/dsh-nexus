# Phase 6 report: release-aware Harness launch

## Worktree and scope

- Task: `nexus-bootstrap`
- Phase: `release-binding`
- Worktree: `E:\git\dsh-nexus-release-binding`
- Branch: `codex/nexus-bootstrap/release-binding`
- Base: `7db8f6fdd1b91838d17f6fba74e381a3dc590689`
- Scope: `nexus-core`, `nexus-agent` supervisor, architecture baseline, and
  this report.
- Exclusions: no Harness source change, no Desktop/Tauri/Electron code, no
  production launch, and no writes under `D:\dsh-local`.

## Delivered

- Added `ReleaseStore::release_root`, which resolves only a registered slot,
  requires its immutable manifest, canonicalizes the path, and verifies it is
  contained below Nexus `releases/`.
- Extended `HarnessLaunchSpec` with explicit `{release}` and `{release_root}`
  rendering alongside `{profile}`. Missing release context is a clear
  configuration error; static launch specifications remain compatible.
- Updated `HarnessSupervisor` to load the current release catalog before every
  start, resolve the selected canonical slot, and render program, working
  directory, and arguments from that context. It never infers a release from
  the process cwd or edits upstream Harness files.

## Verification

- `cargo fmt --all -- --check` — pass.
- `cargo test --workspace --locked` — 26 tests passed, 0 failed.
- `cargo build --workspace --locked --release` — exit 0.
- `git diff --check` — exit 0.
- Core tests cover slot containment, placeholder rendering, missing-release
  rejection, and the existing profile/checkpoint/release invariants.
- Runtime smoke used a fake PowerShell Harness command and a Nexus-owned
  release slot. After register/promote, the supervisor rendered
  `{release_root}`, `{release}`, and `{profile}` and wrote a marker inside the
  selected slot:

  ```text
  REGISTER_STATUS=201
  PROMOTE_STATUS=200
  START_STATUS=200
  HARNESS_STATE=stopped
  STATE_RELEASE=harness-rc1
  MARKER_EXISTS=True
  MARKER_CONTENT=web|harness-rc1
  SHUTDOWN_STATUS=202
  AGENT_EXITED=True
  ```

  No real Harness binary or upstream source was launched.

## Boundaries carried forward

Release promotion is still an explicit stopped-state operation. The update
executor stages a verified slot, but does not auto-promote it or start Harness.
Tauri/Electron shell, diagnostics export, local authentication, and automatic
update policy remain independent layers over the same headless Agent API.
