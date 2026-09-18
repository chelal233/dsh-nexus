> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../../README.md) · [当前文档 / Current documentation](../../../README.md)

# P0 snapshot Agent wiring report

## Scope and result

- Worktree: `<WORKSPACE>/dsh-nexus-phases\p0-snapshot-agent`
- Branch: `codex/nexus-p0/snapshot-agent`
- Fixed base: `f56c5e4b618ec9ee27c3baf2dc0e9dd4bac9da13`
- Result: PASS candidate for fixed-commit review. This phase wires the frozen
  `nexus-snapshots` engine into Agent/protocol/CLI and does not claim that the
  recovery UI, native Profile/plugin operations, cold install, or real Harness
  acceptance is complete.

## API and compatibility

`GET /v1/checkpoints` returns legacy checkpoint manifests, validated snapshot
inspections for the active profile, any pending content restore, and the latest
healthy-capture diagnostic. `POST /v1/checkpoints` accepts `create`, `detail`,
`inspect`, `restore`, `retry`, and `abort`; detail/inspection are bounded
metadata and contain no file bytes. `nexusctl checkpoint` exposes the same
actions, with machine-readable `--json` output. Existing launcher/Tauri routing
already gates the single `/v1/checkpoints` GET/POST path, so no route or body
authority was expanded.

New `CheckpointManifest.snapshot` holds an immutable snapshot ID and summary.
The field is optional: old manifests deserialize without it, remain listable,
and report `legacy_metadata_only` when restored. They never claim content
recovery. Snapshot DTOs expose kind, profile, DSH version, plugin/file counts,
stored bytes, hashes, modes, omitted reasons, and redacted structural paths,
but no content.

`config.json.snapshots` independently bounds the healthy ring (default 3,
range 1..32) and manual snapshots (default 64, range 1..1024). A concrete
Running Harness log-session durably claims one automatic capture attempt. A
failure updates a readable diagnostic and does not stop or restart Harness.

## Restore and materialization order

1. Agent acquires supervisor lifecycle, then updater try-gate, verifies Harness
   is quiescent, and acquires the one snapshot I/O owner. Detached owners retain
   all three across HTTP cancellation.
2. The engine validates and prepares a serializable ticket. Agent binds the
   ticket to the engine's canonical resolved `DSH_HOME` and target profile, then
   durably writes the existing outer `CheckpointRestoreIntent::Prepared`.
3. Agent applies the content ticket. Package, lockfile, or workspace changes
   leave `materialization_pending`; Agent invokes only the configured pinned
   runtime with shared `resolve_runtime_command`, `build_runtime_child_env`, and
   `build_pnpm_args`, using `pnpm install --frozen-lockfile`. Only exit success
   permits `mark_materialized`.
4. Agent publishes target profile/release selection, writes outer `Committed`,
   then asks the engine to commit and clears the journal. An ambiguous outer
   commit result is re-read before choosing rollback or finish.
5. Apply/materialization failure keeps Prepared plus a bounded diagnostic.
   Read APIs remain available; ordinary mutations return HTTP 409. Retry reuses
   the same ticket and resumes apply/materialization. Abort is allowed only for
   Prepared and rolls content and selection back. Committed Retry only finishes
   commit and can never apply again or clear materialization optimistically.
6. Startup validates the persisted home/profile binding. Prepared rolls back;
   Committed validates target selection and finishes commit. Recovery failure
   stays visible and Agent continues serving explicit Retry/Abort/read actions.

The engine's fixed seven-file policy, hashes, structured secret filtering,
in-home undo, and durability rules are unchanged. Integration tests prove a
dummy snapshot secret is absent from DTO serialization and restore preserves
the current protected value. No `.credentials`, `.env`, session, or arbitrary
caller path is accepted or read.

## Owned materialization process tree

On Windows, the direct pinned Node/pnpm child starts suspended, is assigned to a
new kill-on-close Job Object, and resumes only when ToolHelp finds exactly one
thread for the owned child PID. Job creation, assignment, ambiguous thread, or
resume failure kills and reaps the suspended child. Timeout and wait errors
terminate the job and wait for an empty job before returning, so Abort cannot
race a descendant still writing the profile. Unix configures a private process
group and signals only that owned group; this Windows phase does not claim a
Unix real-process acceptance run.

## Verification

- `cargo test --offline -p nexus-protocol -p nexus-core -p nexus-agent`: exit 0;
  Agent 112, core 30, protocol 13, failures 0; doc tests 0.
- `cargo check --offline -p nexus-cli -p nexus-launcher-core`: exit 0.
- `cargo clippy --offline -p nexus-agent -p nexus-core -p nexus-protocol -p nexus-cli --all-targets`
  with repository-existing lint classes explicitly allowed and `-D warnings` for
  the remaining classes: exit 0. The first strict invocation stopped only on
  pre-existing warnings in untouched baseline code.
- Focused Windows tests: timeout returned only after a controlled descendant
  stopped writing; injected job-create, pre-assign, and pre-resume failures left
  the suspended child inert. Fake pnpm verified the exact policy argv,
  `DSH_HOME`, profile cwd, transient failure pending state, same-ticket Retry,
  and Abort.
- Synthetic snapshot tests cover current-secret preservation, bounded detail,
  home/profile ticket binding, healthy/manual retention, durable healthy-once,
  cancelled I/O ownership, and startup Prepared/Committed recovery.

No real DSH home, credentials, sessions, Harness, Corepack, download, system
installation, deployment, or GUI was accessed. Windows junction behavior is
covered by the frozen engine suite, not re-run as a real user-data scenario here.
