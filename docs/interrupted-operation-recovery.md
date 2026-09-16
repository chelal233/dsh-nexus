# Interrupted operation recovery

Nexus must not infer that a subprocess has exited merely because Agent restarted.
Disposable commands persist an ownership record and hold an OS file lease across
queued execution and process creation. Working files may be cleaned only after
the lease and the recorded process tree are quiescent.

On Windows, process creation assigns the Job atomically with
`PROC_THREAD_ATTRIBUTE_JOB_LIST`; closing the owner's Job terminates descendants.
On Unix, the command records its process group before exec and starts a guardian
that kills the group when the Agent pipe closes. The compatibility probe stays
in this group. Recovery queries identity; it never kills an arbitrary reused PID.

| Operation | Interrupted outcome and retry |
| --- | --- |
| Compatibility check / projection publication | Verify process ownership, discard only disposable work, rerun against current configuration. Never replay an old publication. |
| Cold install / offline import or export | Reconcile command ownership before publication recovery or candidate cleanup; retry cleanup when the previous owner finishes. |
| Update installation | Preserve primary failure and candidate while descendants remain; recover named or registry-backed ownership before admitting another install. |
| Canary diagnostics | Set an abandoned operation to interrupted after quiescence; keep cancellation available if shutdown is still in progress. |
| Checkpoint apply / dependency materialization | Both startup and explicit retry/abort check process quiescence before rollback or completion. Existing Prepared/Committed journal remains authoritative. |
| Profile archive | Existing rename/catalog journal recovers; pre-journal empty owned directories are removed without recursive deletion. Nonempty evidence is retained. |
| Configuration / release publication | Existing durable transaction rollback, commit verification, and external-edit conflict checks remain in force. |
| Mutating HTTP request | Existing request receipts report interruption; uncertain side effects are not automatically replayed. |
| Electron update preference | Write and fsync an exclusive temporary file, then replace atomically; interrupted temporary files do not change the previous preference. |
| Harness launch / desktop update handoff | Preserve existing readiness-based startup recovery and OS lock handoff. Harness creation now also has atomic Job assignment on Windows. |

Old records lacking process ownership cannot safely be repaired by deleting a
marker or trusting a dead parent PID. For these records, the migration requires
one computer reboot and verifies that the record predates the current OS boot.
Configuration and work evidence remain intact until this proof is available.
Unknown schemas, changed paths, or denied ownership checks remain visible errors.

Verification includes a real Agent-owner subprocess kill with a live child and
grandchild, successful subsequent command execution, durable-cut compatibility
tests, live-materializer checkpoint rollback exclusion, and interrupted Electron
preference writing. Windows runtime acceptance does not establish macOS runtime
acceptance. Unix process groups cannot contain third-party programs deliberately
escaping the group with a new session; this is not a sandbox for hostile plugins.
