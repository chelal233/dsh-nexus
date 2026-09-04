# Harness restart reconciliation phase report

Task: `nexus-native-launcher`
Phase: `harness-restart`
Base: `dd8656c2e6d909124055c692e18ee4fba62815d5`
Worktree: `E:\git\dsh-nexus-harness-restart`
Branch: `codex/nexus-native-launcher/harness-restart`

## Worktree handshake

`worktree_verified PASS`: the repository root, branch, and base HEAD matched
the declared phase before edits. The worktree already contained the scoped,
uncommitted Harness restart implementation; no unrelated changes were
discarded. Writes remained limited to the five source/document paths and this
report. Harness/Desktop source, `D:\dsh-local`, root Cargo manifests, and lock
files were not changed.

## Delivered

- Added a supervisor lifecycle mutex covering start, stop, and restart. Restart
  uses internal stop/start methods so the guard is not self-locked; status
  polling does not acquire the lifecycle guard.
- Centralized supervisor metadata writes behind generation and complete-runtime
  identity checks. Publications hold the supervisor inner lock through the
  atomic metadata write, and Agent snapshots retry against the current runtime
  before publishing, preventing late recovery/exit data from replacing an
  explicit stopped or newer running state.
- Kept recovery state intact across pre-spawn failures and rejected duplicate
  starts while an unattached recovery or healthy no-PID runtime is active.
- Made the Launcher Harness log observer retain an in-memory bounded tail
  cursor, expose an existing tail on first read, distinguish append from file
  replacement by content continuity, ignore mtime-only touches, and detect a
  longer rotation whose new token precedes the old offset.
- Made the native UI poll Harness `starting`/recovery at roughly 400 ms and
  stable state at eight seconds. Refreshes are coalesced, and a pending flag
  drains a trailing refresh after an action completes during an in-flight
  request.

## TDD and verification evidence

The longer-rotation and concurrent start-during-stop tests were first added
against the existing implementation and failed (old token retained; start
returned before stop completed), then passed after the fixes. Recovery
snapshot, duplicate-start, and spawn-state regression coverage was added in
the same phase.

| Check | Result | Evidence |
| --- | --- | --- |
| Worktree/base handshake | PASS | `git rev-parse --show-toplevel`, `git branch --show-current`, `git rev-parse HEAD`, and `git status --short --branch` |
| Rust formatting | PASS (exit 0) | `C:\Users\PC\.cargo\bin\cargo.exe fmt --all -- --check` |
| Rust workspace check | PASS (exit 0) | `C:\Users\PC\.cargo\bin\cargo.exe check --workspace --locked` |
| Rust workspace tests | PASS (exit 0) | `C:\Users\PC\.cargo\bin\cargo.exe test --workspace --locked`; 21 Agent, 15 Core, 11 Launcher, and 9 Protocol tests passed |
| Frontend typecheck | PASS (exit 0) | bundled Node 24.19.0 PATH + `pnpm typecheck` |
| Frontend production build | PASS (exit 0) | bundled Node 24.19.0 PATH + `pnpm build`; Vite emitted ignored `dist/` assets |
| Diff hygiene | PASS (exit 0) | `git diff --check` |

No deployment, merge, Harness/Desktop modification, `D:\dsh-local` access, or
root manifest/lockfile modification was performed.

## Acceptance disposition

All requested local gates passed. The committed phase is ready for the parent
agent's independent review and adversarial re-audit; merge remains outside
this phase.
