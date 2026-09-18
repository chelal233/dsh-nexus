> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../../README.md) · [当前文档 / Current documentation](../../../README.md)

# P0 runtime supply implementation report

Date: 2026-09-05
Base: `f56c5e4b618ec9ee27c3baf2dc0e9dd4bac9da13`
Scope: isolated `nexus-runtime-supply` library and its workspace/trust/documentation entries. This segment does not wire Agent, API, UI, or cold-clone orchestration.

## Result

The new library plans and executes confirmed Windows runtime supply without accepting caller-provided URLs or commands. `RuntimeSupplyPlanner::plan` derives an exact `SupplyPlan` from a fresh foundation `RuntimePlanResponse`. `RuntimeSupplier::execute_confirmed` requires the exact deterministic `supply_plan_id`, compares the fresh foundation plan, host, destination, and compiled source-policy revision, then re-derives source requests and cache observations before side effects.

Reuse order is existing verified system/user pin, exact read-only Corepack pnpm cache, complete Nexus-owned cache, then confirmed portable or system acquisition. Returned `SupplyOutcome.runtime` contains absolute pins and ownership for the later Agent transaction; this crate does not write configuration.

## Source and artifact trust

- Official and npmmirror source policies construct all URLs internally, require HTTPS/default ports/no credentials, bound redirect count and response size, and allow only the documented Node mirror-to-CDN transition while preserving the exact resource suffix.
- Node selection chooses the highest stable LTS release that satisfies every release engine requirement and publishes the required Windows artifact for the host architecture. A compiled Node release-key set verifies the cleartext-signed checksum manifest, then the exact artifact SHA-256 is streamed and checked.
- pnpm metadata must match the exact package/version/bin entry. A compiled npm ECDSA key verifies `name@version:integrity`; the tarball must match that SHA-512 SRI.
- Existing Corepack cache discovery is read-only, exact-version and content-identity bound, canonical/reparse contained, and verifies `pnpm.mjs` through the selected Node. It never executes a Corepack shim or allows Corepack to download.

The fixed trust inputs and provenance are recorded in `crates/nexus-runtime-supply/trust/README.md`. No runtime `HEAD` key retrieval becomes a trust root.

## Portable and publication behavior

ZIP and tar extraction accept only bounded regular files/directories under the expected root. They reject absolute/traversal paths, links/devices/reparse points, Windows alternate-stream or unsafe names, duplicate/case collisions, file/directory collisions, excess entry count, per-file size, and total size. Files are written create-new and synced. A complete cache manifest binds tool/version/entry hash/artifact identities. On Windows, extracted files and the manifest are synced through their file handles, then the same-volume directory is published with `MoveFileExW(MOVEFILE_WRITE_THROUGH)`. Windows directory handles are not passed to `FlushFileBuffers`; a publication API error is returned rather than treating an unsupported directory flush as durable. Non-Windows publication retains directory sync before and after atomic rename.

A process-local mutex plus a Windows OS-exclusive file handle serializes publication. The persistent marker can be reopened after a crashed owner exits, so a stale filename cannot permanently block supply. Confirmation rechecks complete owned cache after taking the lock, so concurrent requests reuse one valid publication. Cancellation/failure removes only the request-owned staging directory and never publishes a partial tree.

## Process and system behavior

All probes use the foundation `resolve_runtime_command` and child-only environment builder, including Node plus a pnpm `.mjs` entry. Windows owned probes are created suspended, attached fail-closed to a kill-on-close Job Object, and resumed only after attachment succeeds. Attachment, thread discovery, open, or resume failure kills and reaps the still-suspended child. The owned Job is nested under an inherited host Job when present, avoiding a breakaway requirement while retaining Nexus process-tree ownership. The job is closed before bounded stdout/stderr readers are joined, so descendants cannot retain the pipes after timeout, cancellation, or direct-parent exit.

System mode produces private typed specifications only: verified Node MSI through absolute System32 `msiexec.exe` with `/passive /norestart ADDLOCAL=NodeRuntime`, and the pinned official pnpm user install script through absolute System32 Windows PowerShell with exact `PNPM_VERSION`, child-local `PNPM_HOME`, and registry. Success, reboot-required, user cancellation, UAC denial, spawn failure, ordinary failure, timeout, and cancellation have distinct results. Timeout/cancellation after system launch returns `NeedsVerification`; Nexus does not kill global installer services or publish an unverified pin. A successful runner result is followed by exact version observation of the expected absolute path.

The pnpm system script itself obtains pnpm using its upstream implementation. Nexus binds and verifies the pinned script digest and exact requested version and verifies the installed result; npm package SRI authenticates the portable tarball and is not represented as authenticating the script's separate standalone payload.

## Verification

- `cargo test --offline -p nexus-runtime-supply`: 17 passed, 0 failed. Coverage includes confirmation/stale/tampered plans, strict redirect identity, real Node signed-manifest fixture, real pnpm metadata signature/SRI, Corepack content change, ZIP traversal/case collision, tar regular extraction/link rejection, corruption, cancellation cleanup/no publication, concurrent cache reuse, stale lock-marker recovery, deterministic LTS selection, typed system plans, and system result classification. Windows-specific regressions exercise a fresh synthetic portable tree through manifest creation, parent creation, write-through rename, and published-content readback; and hold an owned process after suspended creation to prove no fixture code runs before Job attachment, then verify its controlled descendant is gone after Job close.
- `cargo clippy --offline -p nexus-runtime-supply --all-targets --no-deps -- -D warnings`: passed.
- The same Clippy command without `--no-deps` reaches pre-existing `nexus-protocol` lints outside this phase's scope (seven `too_many_arguments`/`derivable_impls` findings); no runtime-supply lint remained.
- `cargo fmt -p nexus-runtime-supply`: applied; the crate is formatted.

Dependency source packages needed by the new crate were resolved into Cargo's cache; final verification was offline. No Node/pnpm artifact body, MSI, installer script execution, real system installation, PATH mutation, Corepack execution, real `DSH_HOME`, Harness launch, or GUI acceptance occurred.

## Downstream contract and remaining P0 work

The cold orchestration lane should hold a server-owned `SupplyPlan` plus its token, release its lifecycle/update locks while awaiting confirmation, regenerate the foundation plan on confirmation, then call `execute_confirmed`. It must commit `SupplyOutcome.runtime` through the existing shared config transaction. Install/build/start/materialization/plugin/terminal consumers must continue to use the single foundation runtime command, child-env, and pnpm-argument builders.

Agent endpoints, persisted operation state, UI confirmation, cold clone/install/build/promotion, real portable reuse/build-to-launch acceptance, and explicitly authorized real system acceptance remain outside this segment.
