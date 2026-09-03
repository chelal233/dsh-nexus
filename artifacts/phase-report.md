# Phase 4 report: profiles, checkpoints, and release slots

## Worktree and scope

- Task: `nexus-bootstrap`
- Phase: `release-slots`
- Worktree: `E:\git\dsh-nexus-release-slots`
- Branch: `codex/nexus-bootstrap/release-slots`
- Base: `316c8f7583affc2a285cf485d88ade416b79bd94`
- Scope: the Rust workspace, `docs/architecture-baseline.md`, and this
  report only.
- Exclusions: no Harness/Desktop/Tauri source, no `Cargo.lock` changes, no
  writes under `D:\dsh-local`, no deployment, and no real Harness launch.

## Delivered

- Preserved the stable v1 JSON types for profile and checkpoint operations and
  added release list/current/register/promote/rollback commands. Release IDs,
  versions, and text metadata are bounded and validated before persistence.
- Added Nexus-owned immutable release manifests under
  `releases/<id>/manifest.json`. Registration is append-only metadata: it does
  not download, build, overwrite, or launch Harness.
- Added an atomic `release-pointers.json` document with `current_release` and
  `last_known_good` pointers. Promoting a registered slot moves the previous
  current pointer to LKG; rollback swaps the two pointers so the operation is
  reversible and no manifest is modified.
- Agent boot now reads the release pointer and keeps `AgentState.release` and
  `state.json` metadata aligned. `GET|POST /v1/releases` exposes the catalog;
  promotion and rollback return HTTP 409 while Harness is starting/running and
  update Agent metadata only while stopped.
- Checkpoint restore validates a saved release against the registered catalog
  before applying profile/release metadata, preventing a pointer to an
  unavailable slot. It still never copies or rewrites `.dsh` data.
- Extended `nexusctl` with `release list|current`, `release register ID VERSION`,
  `release promote ID`, and `release rollback`, with optional `--source`,
  `--note`, `--json`, and `--port` options.
- Updated the architecture baseline to describe immutable release slots,
  atomic current/LKG pointers, and the remaining external update executor,
  authentication, diagnostics, and Tauri boundaries.

## Test-first and verification

Final commands were run from this worktree with Rust 1.98.0:

- `cargo fmt --all -- --check` — exit 0.
- `cargo check --workspace --locked` — exit 0.
- `cargo test --workspace --locked` — exit 0; 24 unit tests passed, 0 failed.
- `git diff --check` — exit 0.
- Fake Nexus store tests covered default/selected profiles, checkpoint
  manifests, and release registration/promotion/rollback without launching a
  real Harness. No external `.dsh` data was read or written.

Runtime/CLI smoke used an ignored temporary Nexus root and a fake
`powershell.exe Start-Sleep` child; no real Harness was launched:

```text
nexusctl release list --json                 -> empty catalog, no pointers
nexusctl release register harness-alpha5 ...  -> immutable manifest created
nexusctl release register harness-rc1 ...     -> second manifest created
nexusctl release promote harness-alpha5       -> current=alpha5
nexusctl release promote harness-rc1          -> current=rc1, LKG=alpha5
nexusctl release rollback                     -> current=alpha5, LKG=rc1
nexusctl status --json                       -> state.release=alpha5
nexusctl harness start                       -> fake Harness running
nexusctl release promote harness-rc1         -> HTTP 409 release_change_conflict
nexusctl harness stop                        -> bounded stop completed
POST /v1/shutdown                            -> 202; Agent exited
```

## Not done / boundaries

- Nexus does not discover or mutate Harness source, credentials, sessions, or
  its data directory. HTTPS readiness, non-loopback readiness, and remote
  Agent binding are intentionally unsupported.
- Checkpoint restore applies only Nexus profile/release metadata; it does not
  restore Harness sessions, credentials, user files, or `.dsh` state.
- Profile names are metadata and are rendered only where an explicit
  `{profile}` placeholder is configured. Nexus does not guess a Harness CLI
  flag or internal implementation.
- The external update executor (git/download/build/readiness), authentication,
  diagnostics, Tauri/Desktop integration, and remote control remain future
  phases. Release registration and pointer swaps are metadata-only and do not
  yet bind a selected slot to the Harness launch command.
