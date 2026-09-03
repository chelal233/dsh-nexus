# Phase 5 report: external update executor

## Worktree and scope

- Task: `nexus-bootstrap`
- Phase: `update-executor`
- Worktree: `E:\git\dsh-nexus-update-executor`
- Branch: `codex/nexus-bootstrap/update-executor`
- Base: `6556ba3e0784a77c801e1d5b625317cf8918e1d8`
- Scope: Rust workspace, `docs/architecture-baseline.md`, and this report.
- Exclusions: no Harness/Desktop/Tauri source, no Harness fork or vendoring,
  no writes under `D:\dsh-local`, no deployment, and no real upstream clone.

## Delivered

- Added versioned v1 update protocol types and `GET|POST /v1/updates`.
- Added Nexus-owned `UpdateSpec` configuration with bounded source/ref/program
  validation, credential-bearing URL rejection, direct argument vectors, and
  `{source}`, `{release}`, and `{ref}` placeholders.
- Added a serialized `UpdateExecutor`: it clones a selected Git ref into a
  temporary `downloads/` candidate, runs optional build and verify commands with
  a bounded timeout, and publishes the candidate through
  `ReleaseStore::register_prepared` only after those hooks succeed.
- Added atomic `update-state.json` persistence and restart recovery: an old
  `running` record is marked failed because a new Agent cannot attach to the
  old child process. Update stdout/stderr are kept in release-specific Nexus
  log files without logging command arguments or credentials.
- Kept release slots immutable: prepared candidates are contained below
  `downloads/`, renamed into `releases/<id>`, and receive their manifest last.
  The executor never edits the upstream source tree or changes current/LKG
  pointers implicitly.
- Extended `nexusctl update status` and `nexusctl update install [ID VERSION]`;
  omitted values use the configured ref and a safe generated release ID.

## Verification

Commands run with Rust 1.98.0:

- `cargo fmt --all -- --check` — exit 0.
- `cargo check --workspace --locked` — exit 0.
- `cargo test --workspace --locked` — exit 0; 25 unit tests passed, 0 failed.
- `cargo build --workspace --locked --release` — exit 0.
- `git diff --check` — exit 0.

Runtime smoke used a disposable local Git fixture and a separate ignored Nexus
root. The update API cloned `main`, ran PowerShell build/verify hooks, moved the
candidate into an immutable slot, persisted success, and shut the Agent down:

```text
HEALTH=ok
INSTALL_STATUS=201
UPDATE_STATE=succeeded
RELEASE_COUNT=1
RELEASE_ID=harness-smoke
RELEASE_VERSION=rc.1
BUILD_MARKER=True
SHUTDOWN_STATUS=202
AGENT_EXITED=True
```

The resulting slot contained `README.md`, `build.ok`, and `manifest.json`; the
temporary candidate was gone. No `.dsh` path, credentials, or real Harness
source was read or written.

## Boundaries carried forward

The update operation stages and verifies a release but does not silently
promote it or launch Harness. Promotion remains an explicit stopped-state
operation so rollback stays reversible. Tauri/Electron, authentication,
diagnostics bundle export, launch binding to a selected release root, and
automatic update policy remain subsequent phases against this headless API.
