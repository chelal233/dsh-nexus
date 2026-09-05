# Architecture handoff report

Task: `nexus-takeover`
Phase: `architecture-handoff`
Owner: `verify_checkout`
Base ref: `df3cff00cb5e740f223897756f2c7f5bf9c44207`
Branch: `codex/nexus-takeover/architecture-handoff`
Worktree: `E:\git\dsh-nexus-phases\architecture-handoff`

## Worktree handshake

`worktree_verified`: root `E:/git/dsh-nexus-phases/architecture-handoff`,
branch `codex/nexus-takeover/architecture-handoff`, and `HEAD` equal to the
declared base `df3cff00cb5e740f223897756f2c7f5bf9c44207`. The worktree was
clean before the document changes.

## Scope and sources

The write scope is exactly:

- `docs/architecture-baseline.md`
- `artifacts/takeover/architecture-handoff-report.md`

The review source was the existing architecture document in this worktree and
the phase handoff requirements. No main worktree, other worktree, memory,
source code, business data, or runtime output was read.

## Current baseline recorded

- Launcher is the UI shell for Agent business operations. Agent owns business
  state and lifecycle. Harness upstream source is not modified.
- Profile currently uses the Nexus catalog as a legacy metadata bridge. The
  confirmed native Profile create/switch behavior is still a target and has no
  delete operation in this scope.
- Node is the only runtime path tested for this handoff. The legacy direct
  parser remains retained; removing it is outside this phase.
- The current Agent loopback listener is `127.0.0.1:3090`.
- `1e1838b` records tag enumeration, `ec905f3` records slot
  capacity/protection/release, and `df3cff0` records explicit switch with
  automatic install/promote. These commits do not prove cold-install
  build-to-Node-launch acceptance. Ordinary install remains install-only and
  does not automatically promote.
- The P0-4 runtime draft is in an independent uncommitted phase. Runtime
  discovery, portable download, path pinning, source switching, and install
  confirmation remain unverified.
- The current switch path has a lifecycle early-release and cancellation-owner
  handoff risk. It remains unaccepted and was not changed by this phase.

## Confirmed target, pending implementation and acceptance

- Snapshots contain only declarative `profile` and `config` data, with no
  credentials or sessions. Healthy startup uses N rotations (default 3) plus
  a manual journal. The current implementation remains manifest-only.
- Runtime installation prefers a portable runtime below the Nexus
  `data-root/runtimes` directory and passes its location through child-process
  environment without changing system `PATH`. System mode is maintained by
  the system after installation; real-machine acceptance remains with the
  user.
- Node `>=25` does not bundle Corepack. The official source is
  <https://github.com/nodejs/corepack>; documentation must not claim that all
  Node installations include Corepack or that pnpm requires no download.
- Git only guides installation on the local machine. System-install tests are
  prohibited in this phase, and an already available runtime should be reused
  to avoid extra downloads whenever possible.
- Agent obtains a free loopback port from the OS (`port 0`) and Launcher
  discovers the endpoint by identity. The internal port remains out of the UI.

## Acceptance boundary

This phase is a documentation and Git cross-check. It is not runtime
acceptance. Cold-install build-to-Node-launch, runtime discovery, downloads,
path pinning, source switching, install confirmation, snapshot rotation and
journal behavior, and the target port identity flow remain open. No code was
changed and no compile or test command was run.

Verification for this phase is `git diff --check`; the report and baseline
document are committed only after that check passes. The phase does not merge,
deploy, delete its worktree, or modify the main worktree.
