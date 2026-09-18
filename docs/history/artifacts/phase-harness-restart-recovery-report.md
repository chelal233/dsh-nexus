> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../README.md) · [当前文档 / Current documentation](../../README.md)

# Harness restart recovery phase report

Date: 2026-09-04

Base: `dd8656c2e6d909124055c692e18ee4fba62815d5`

Branch: `codex/nexus-native-launcher/harness-restart`

## Outcome

The restart/recovery phase now treats a Harness bootstrap child and a recovered
replacement as different ownership states. Start persists `starting` and
returns promptly; a detached generation-bound task owns readiness and continues
after the initiating HTTP request returns. If that request is cancelled before
ownership is persisted, or the first metadata publication fails, exactly one
detached cleanup owner kills/reaps the spawned child and acknowledges completion.
Child polling and the monitor remain gated until the initial `starting` ownership
snapshot is durable, so a short-lived bootstrap cannot replace that snapshot or
mask its persistence error. If the gated child already exited non-zero and a
readiness target exists, the same owner preserves and schedules unattached
replacement recovery instead of discarding a healthy descendant.
Stop also moves its Child into a detached owner before the API awaits it. A
cancelled stop request therefore cannot lose the handle; start is rejected
while stop is pending, and wait/kill errors restore the Child plus its monitor
and pending readiness owner for a retry. Restoring a `starting` child re-arms
the ownership gate until that readiness owner accepts the already-durable
snapshot, so the owner can still publish `running` when the endpoint recovers.
Readiness never holds the lifecycle mutex, so stop is bounded by the process
stop grace period rather than the readiness timeout.

When a bootstrap parent exit (including code 0) is followed by a healthy configured loopback
endpoint, the supervisor reports `running` with `pid: null` and clears the stale
exit/error fields. Pending recovery and recovered running are both unattached:
neither can be truthfully stopped or started again by this Agent. Stop returns
`409 harness_unattached`, retains the duplicate-start guard, and start continues
to return `harness_already_running`. Completed child exits, explicit stops, and
persisted unattached stopped/failed snapshots clear the no-longer-live PID while
retaining historical exit information.

Agent metadata publication now has two independent freshness gates: Harness
generation/current-runtime equality and a monotonic Agent revision. All
post-startup Agent runtime mutations are serialized through the same revision
domain. A stale Agent snapshot therefore cannot overwrite a newer profile,
release, lifecycle, or Harness publication.

The Tauri proxy keeps `/launcher/*` on the native shell API and resolves every
`/v1/*` request through the current loopback `agent_api` advertised by
`/launcher/status`. Advertised origins are restricted to canonical plain HTTP,
an explicit non-zero port, and loopback hosts, with credentials, paths, query,
fragment, HTTPS, and remote hosts rejected.

Every fresh Harness spawn now creates unique Nexus stdout/stderr files below
`logs/`, atomically writes and syncs `run/harness-log-session.json`, persists a
write-ahead `starting` intent with no PID, and only then creates the child. The
marker carries a unique run ID, generation, safe per-run filenames, per-file
byte watermarks, open-file identities (Windows volume serial/file index or Unix
device/inode), and an active ownership reservation. A crash on either side of
spawn therefore cannot expose an older terminal snapshot as permission for a
duplicate start. `/v1/harness` publishes the corresponding supervisor generation
and complete log-session boundary. Launcher requires two stable `running`
observations that match the marker, parses only complete URL words beginning at
or after the watermark using raw byte offsets, and rechecks both runtime and
marker after parsing. Missing/corrupt markers, non-running or unreachable Agent
state, mismatched writers, truncated or same-path replaced logs, and
state/session changes fail closed with no URL/token. Invalid UTF-8 is skipped as
raw bytes and cannot shift candidate offsets across a watermark. An ordinary
append cannot resurrect a pre-session token, while a current token remains
reconstructible after Launcher restart. The bootstrap parent and its readiness
descendant are one logical start/run and therefore retain one session marker. A
recovered `running` state with no PID is continuously probed; loss advances the
same per-run files to a new run ID, generation, and EOF watermark before bounded
recovery. Readiness may return to `running`, but its URL/token remains
unavailable until new output appears after that boundary. An explicit fresh
start/restart always creates a new pair of log files and advances the session.

The Harness URL observer also no longer infers append safety from a matching
bounded overlap. Any changed length/content reparses the complete current 64 KiB
tail and discards tokens that are no longer present. The native UI performs a
coalesced trailing refresh after both successful and failed actions. Token,
copy, open, reveal, and iframe state are additionally gated on the independent
Harness runtime being `running` and on an exact generation/run-ID join, so a
mixed refresh cannot render stale credentials.
Every recovery handoff schedules its detached owner before the first subsequent
await. Cancelling a status request during metadata persistence therefore cannot
leave `task_started` set without a task to finish or time out the recovery.

Launcher bootstrap no longer uses a timestamp-based delete/create lock. It
holds an OS-exclusive lock on a persistent `run/agent.lock` inode while checking
and starting Agent. Agent health contains the canonical data-root identity and
a per-process instance ID. Every adoption and shutdown probe verifies the data
root; launch records correlate to the current instance, so a service using the
same port for another Nexus root is neither adopted nor stopped. On Unix,
atomic JSON replacement also syncs the parent directory after rename.

The bounded token parser reads the byte immediately before its tail and
requires both a left delimiter and an observed right delimiter. A URL cut by
the 64 KiB boundary or ending in an incomplete EOF fragment is never published.

## Regression coverage

- API request cancellation before initial persistence cannot leave a live
  attached child or permanent `starting` state.
- Status polling cannot observe a short-lived child while initial ownership is
  pending. Cancellation then preserves healthy replacement recovery, rejects a
  duplicate start, and keeps the bootstrap marker at one.
- A short-lived bootstrap cannot mask an initial metadata write failure; the
  caller receives the persistence error while in-memory replacement recovery
  remains duplicate-safe.
- Start returns before readiness and the independent owner later publishes
  `running`.
- Stop completes promptly while a 60-second readiness deadline is pending.
- Cancelling stop cannot drop the Child; its detached owner completes, while a
  concurrent start is rejected.
- A simulated stop wait error restores the Child handle and permits a safe
  readiness transition to `running`, followed by a safe stop retry without
  duplicate bootstrap.
- Initial runtime-metadata failure after spawn reaps the child and clears PID.
- Pending recovery with a live HTTP endpoint and unattached `running` both
  reject stop without fake state changes and reject duplicate start.
- A second start that observes a short-lived bootstrap exit schedules the
  existing replacement recovery, keeps the bootstrap marker at one, and reaches
  unattached `running` without launching another Harness.
- Cancelling status while a newly observed recovery snapshot is waiting to
  persist cannot strand the handoff; the already-scheduled owner still probes
  readiness and reaches unattached `running`.
- Stale Harness generations and stale Agent revisions are rejected.
- Ordinary non-zero process exit and explicit stop clear stale PIDs.
- Agent startup scrubs stale PIDs from stopped and failed persisted snapshots.
- Non-zero bootstrap recovery preserves the real exit code on timeout and
  clears it on successful replacement readiness.
- A code-zero bootstrap parent is treated identically for readiness recovery;
  its live descendant reaches `running` and a second start never executes.
- A durable prepared marker combined with an older terminal state is recovered
  after Agent restart without executing the configured Harness program again.
- If the prepared runtime snapshot cannot be persisted before spawn, no child
  is created and the durable reservation is released; a reservation-release
  failure remains fail-closed instead of permitting a duplicate start.
- A recovered PID-less process is continuously checked; liveness loss advances
  the token epoch before recovery and keeps duplicate start rejected.
- If the original readiness owner wins the parent-exit/recovery race, publishing
  PID-less `running` atomically claims the same liveness monitor before its
  first await; subsequent endpoint loss still advances the token epoch.
- A 65,536-byte log replaced by a longer sliding-window file cannot retain a
  removed old token.
- Missing and corrupt durable log-session markers fail closed even when old
  logs contain a token.
- A `starting` or otherwise non-running Agent observation clears the token and
  URL; returning to `running` without output after a new watermark remains
  unavailable.
- Ordinary non-token output cannot resurrect a pre-session token; a newly
  appended token is accepted, and a new Launcher observer reconstructs only the
  current run's token.
- Cross-watermark URL words and files shorter than their recorded watermark are
  rejected.
- A same-path replacement of equal or greater length is rejected by open-file
  identity, and invalid UTF-8 cannot shift an old token across a byte watermark.
- Fresh sessions read unique per-run filenames, so a longer copy-truncate of
  the reusable legacy log path cannot inject an old token into the current run.
- Tail-leading URL fragments and EOF-undelimited token fragments are rejected;
  the latter becomes eligible only after its delimiter is observed.
- Concurrent launchers cannot acquire the same OS file lock, and a foreign
  data-root Agent health response is rejected before adoption or shutdown.
- The fresh-start marker records EOF from the exact log handles before child
  output, while bootstrap-parent readiness recovery retains the same logical
  session marker.
- An Agent observation whose log run ID/generation does not match the durable
  marker is rejected, covering an unsupported second writer without exposing a
  mixed token.
- A supervisor whose durable marker was replaced by another writer refuses to
  spawn Harness instead of racing a last-writer-wins update.
- Tauri routes Agent and Launcher endpoints to different ports and rejects
  unsafe advertised Agent origins while accepting explicit HTTP port 80.

## Verification

- `cargo fmt --all -- --check`
- `cargo check --workspace`
- `cargo test --workspace`
  - `nexus-agent`: 40 passed
  - `nexus-core`: 16 passed
  - `nexus-launcher`: 22 passed
  - `nexus-protocol`: 9 passed
- `cargo test -p nexus-agent --lib --quiet` repeated 10 times: 400/400 passed
- `cargo fmt --manifest-path apps/nexus-launcher/src-tauri/Cargo.toml -- --check`
- `cargo check --manifest-path apps/nexus-launcher/src-tauri/Cargo.toml`
- `cargo test --manifest-path apps/nexus-launcher/src-tauri/Cargo.toml`: 4 passed
- `pnpm --dir apps/nexus-launcher typecheck`
- `pnpm --dir apps/nexus-launcher build`
- `git diff --check`

## Boundaries

No DeepSeek Harness source or data is modified. `D:\dsh-local` is outside the
phase and was not accessed. Tests cover the control-plane state machine, proxy
routing, log rotation, and frontend compilation. A packaged clean-machine
Tauri/WebView2 launch and prolonged real Harness self-restart soak remain
release acceptance activities rather than claims of this phase.

The supported process model has one Launcher-managed Agent per Nexus data root;
the OS-owned `run/agent.lock` serializes concurrent bootstrap attempts and
Agent health binds adoption to the canonical data root. The Agent response/marker
identity check fails closed if another writer changes the marker, but manually
starting independent Agents on different ports against the same data root is
outside the supported contract because their child log streams cannot be
separated after the fact.
