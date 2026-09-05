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

## Cold failure convergence closure (2026-09-05)

Real acceptance found operation `cold-1788601658283382700` stranded at
`cloning / 10%` after the Git child had exited. The original Git cause remains
unrecoverable because that build discarded stderr. This fix does not guess it.

Cold Git clone/revision and pnpm install/build now write stderr to unique
server-owned files below the Nexus run directory. The owner reads at most a
64-KiB tail after the process tree has settled, drops a partial first line when
truncated, applies the existing diagnostics credential redaction, reports the
phase and exit status, and removes the temporary file. Pipes are not used, so a
verbose child cannot deadlock on an unconsumed buffer.

Failure persistence is independent of candidate cleanup. `error` retains the
primary command/orchestration failure; `cleanup_error` retains the secondary
cleanup or reconciliation failure. `owner_quiescent` records whether the owned
process primitive proved the tree empty, while `cleanup_pending` keeps cold and
shared mutation gates closed. A failed operation is therefore terminal and
diagnosable instead of remaining in `cloning`. Explicit cancel retries a
quiescent pending cleanup. Startup treats the former in-memory owner as
quiescent, retries the bounded cleanup, and starts with the primary failure
still intact if residue remains. A new begin cannot overwrite pending residue.

Candidate deletion still accepts only an exact direct child of the canonical
server-owned parent. It uses `symlink_metadata`, detects Windows reparse points,
unlinks link objects with a restricted directory/file fallback, never walks the
link target, and clears the Windows read-only attribute only on an owned regular
file or empty directory before one retry. Entry count and 30-second bounds
remain. Errors include entry kind, exact path and raw OS error when available.

Synthetic Windows evidence:

- Directory and file symlinks targeting both inside and outside the candidate
  were removed with the candidate; both outside targets remained byte-for-byte
  present. A read-only checkout-shaped file was also removed.
- A failing real `cmd.exe` child produced a visible stderr sentinel and a
  credential-shaped line. The terminal error retained the sentinel and exit
  result, emitted `[REDACTED]`, omitted the secret, and left no diagnostic temp
  file.
- An injected containment cleanup failure persisted `Failed`, the primary
  error, separate actionable `cleanup_error`, `owner_quiescent=true`, and
  `cleanup_pending=true`; cancel retried without clearing the evidence, begin
  stayed blocked, and restart cleared a repaired read-only candidate and then
  admitted the next begin.
- `cargo test --offline -p nexus-agent -p nexus-core -p nexus-protocol
  --no-fail-fast`: PASS (Agent 128, Core 30, Protocol 13; doc targets passed).
  `git diff --check`: PASS.

No running Agent, listener, acceptance candidate, real upstream, real DSH home,
runtime installation, Harness process, release registration, publication,
promotion, GUI, merge, or deployment was touched. Remaining acceptance is a new
isolated real cold switch after the root owner replaces/restarts the Agent. It
must either publish the requested tag or reach a terminal failed/cancelled
operation with bounded redacted stderr; any cleanup failure must remain visible
in `cleanup_error` with `cleanup_pending=true`, and process/listener checks must
confirm no descendant or unintended target was affected.

## Real Windows cold acceptance: checkout path correction

Real isolated clone of the approved dsh-v0.1.2-alpha.3 tag exposed Git checkout error 128 (Filename too long) under the owned candidate path. Cold clone now passes command-local `-c core.longpaths=true`; no global Git setting is changed. Repeating the actual clone reached AwaitingConfirmation with verified revision dd6322d604e00eec1ba5e0c8541159906a21094a. The real supply plan reused Node24.19.0 and exact Corepack-cache pnpm11.7.0. Installation/build and Harness/UI acceptance remain in progress; this is checkout acceptance only.
Real pnpm 11.7 acceptance then exposed `Unknown option: registry` in the run/build command. Shared source flags now use `--config.registry=...`, accepted by the actual pnpm run parser and preserving child-local official/npmmirror policy. Shared runtime args and materializer focused tests pass; Agent/CLI build passes. Full install/build acceptance continues.
Real install/build completed but post-build revision validation exposed a lost Git pin: the Node/pnpm-only supply result replaced the full resolved runtime. Confirmation now carries forward the already verified Git pin before build/publication. Eight cold regression tests passed. The persisted owner_quiescent field is reset when confirmation claims a running owner. Native GUI checks also exposed an empty-home profile inventory error; missing home/profiles now return an empty read-only inventory, with a focused missing/invalid-directory regression. Actual publication acceptance remains in progress.
