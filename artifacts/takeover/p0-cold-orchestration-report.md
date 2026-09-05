# P0 cold-install orchestration report

Date: 2026-09-05
Base: `ff476d7706394cdc4389d3666104a5cf5a39072c`

## Result

The Agent now owns one durable asynchronous tag-switch operation. The operation
records its selected tag, source/mode, unique candidate, bounded progress,
foundation plan, server-owned runtime-supply plan and terminal result. `switch`,
`confirm`, and `cancel` are compatible extensions of `/v1/updates`; legacy
status/install remain. `nexusctl` exposes the same actions.

First-run tag reads use the approved deepseek-harness upstream without writing
configuration. Missing Git is reported and never auto-installed. New tags reject
full capacity before clone, clone once below Nexus downloads, load only bounded
candidate manifests, reuse compatible exact runtimes or request confirmation for
the reviewed runtime-supply plan, then run frozen pnpm install/build through the
shared runtime command/environment policy.

Publication verifies the package `bin` mapping and built
`apps/cli/lib/bin.js`. Agent revalidates before acquiring supervisor lifecycle
then updater ownership, rechecks Harness/capacity, registers the immutable slot,
persists absolute runtime pins and Node CLI launch spec with `--profile
{profile}`, promotes, and synchronizes Agent current-release state. Harness is
not started. Cancellation/failure removes an owned candidate and cannot promote;
an Agent synchronization failure restores current/LKG.

## Verification

- `cargo test --offline -p nexus-protocol -p nexus-core -p nexus-runtime-supply -p nexus-agent -p nexus-cli --no-fail-fast`: passed (Agent 114, Core 30, Protocol 13, runtime-supply 17; CLI/doc targets passed).
- CLI compilation is included in the command above.
- Strict Clippy reaches the documented pre-existing `nexus-protocol`,
  `nexus-core`, and Agent foundation style lints before it can be used as a
  clean phase gate; no new warning was accepted as verification evidence.

No real upstream clone/build, runtime download, MSI/script execution, Corepack
execution, Harness start, original DSH home access, system PATH change, GUI
interaction, merge, or deployment occurred. Real isolated portable build and
Harness acceptance remains with the root acceptance phase.
