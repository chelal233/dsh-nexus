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
not started. Before the durable commit decision, cancellation/failure removes
the owned candidate and restores the previous selection/configuration.

## Cold blocker closure (2026-09-05)

Fix base: `b9d25b111ddbf1d984404a9d08df027f9c006c29`.

Publication now persists `cold-publication.json` before register/rename, config,
or pointer mutation. The intent captures the previous config, current/LKG pair,
updater state, target config, operation/release identity and new-slot ownership.
`committed` is the sole durable decision, written after pointer and Agent sync.
Prepared recovery restores the previous tuple and removes the owned slot,
including rename-before-manifest residue. Committed recovery preserves the new
selection and replays updater/operation success. Thus an error after commit is
reconciled as success, rather than falsely reporting that publication failed.
Replay is idempotent and keeps its record on errors. Live rollback reacquires
lifecycle/updater ownership, synchronizes Agent state, and only then persists
the final operation and removes the intent. Startup reads its initial release
catalog after cold recovery. Pending intents block new cold and shared gated
mutations until reconciliation.

Cold Git clone/revision and pnpm install/build now call the existing native
owned-process primitive through a joined blocking owner, with cancellation
polling. Windows starts suspended, assigns the child to a kill-on-close Job,
then resumes it; Unix uses the existing process group. Cancel and timeout reap
the tree before returning. `Cancelling` is nonterminal, and independent owner
state rejects another begin even if a terminal state file already exists.
Candidate deletion checks containment and has a 30-second/two-million-entry
bound. Cleanup errors retain ownership/nonterminal or pending-intent state.

Synthetic evidence:

- Final `cargo test --offline -p nexus-agent -p nexus-core -p nexus-protocol
  -p nexus-cli --no-fail-fast`: PASS (Agent 120, Core 30, Protocol 13;
  CLI and selected doc targets passed). `git diff --check`: PASS.
- Ten restart cuts: intent, rename without manifest, registration, config,
  Promoting phase, pointers, Agent sync, commit decision, updater write, and
  final cold operation write. Repeated recovery checks config/current/LKG,
  Agent state, updater/operation outcome, candidate removal and slot count.
- The cold command path runs a PowerShell parent spawning a writing descendant.
  Both cancellation and timeout prove descendant quiescence, joined command
  completion, candidate removal and second-operation exclusion before owner
  release. Existing suspended-start Job setup failure tests remain applicable.
- Unix group behavior is compiled conditionally but was not executed on this
  Windows host. No real upstream, runtime installer, DSH home or Harness was used.

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
