> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../../README.md) · [当前文档 / Current documentation](../../../README.md)

# P0 snapshot content engine report

## Scope and baseline

- Task / phase: `nexus-p0` / `snapshot-fix1`
- Status: PASS candidate for independent fixed-commit review.
- Worktree: `<WORKSPACE>/dsh-nexus-phases\p0-snapshot-fix1`
- Branch: `codex/nexus-p0/snapshot-fix1`
- Fixed base: `0b8fb8110b921ba22122270338b89bd1e25f2536`
- Scope: bounded follow-up changes in the snapshot crate's `lib.rs`, `validation.rs`, `restore.rs`, and `tests.rs`, plus this report.
- Changed paths: `crates/nexus-snapshots/src/lib.rs`, `crates/nexus-snapshots/src/validation.rs`, `crates/nexus-snapshots/src/restore.rs`, `crates/nexus-snapshots/src/tests.rs`, and `artifacts/takeover/p0-snapshot-engine-report.md`.
- This phase does not wire Agent routes, the healthy-start hook, recovery UI, Harness execution, or pnpm materialization.

## Public content API

`SnapshotStore` takes an explicit canonical Nexus data root, canonical DSH home, and validated profile name. The roots must be disjoint so secret-bearing undo files cannot enter the Nexus snapshot tree.

- `capture_healthy` writes the default three-slot healthy ring; a configurable 1..32 slots is supported.
- `capture_manual` retains manual snapshots outside the healthy ring, with a default bound of 64.
- `list`, `detail`, and `inspect` are bounded. `list_inspections` reports corrupt slots per item so one bad slot cannot hide usable ones.
- Summaries expose DSH version, `dsh.profile.bundles` count, captured file count, stored byte count, kind, time, and ID. They never expose file content.
- Public manifest/ticket/result DTOs use Serde and are independent of `nexus-core`; the Agent can map them into protocol DTOs without a dependency cycle.

The fixed file policy is:

| Scope | Relative path | Maximum |
| --- | --- | ---: |
| profile | `package.json` | 1 MiB |
| profile | `pnpm-lock.yaml` | 32 MiB |
| profile | `pnpm-workspace.yaml` | 1 MiB |
| profile | `cordis.patch.yml` | 1 MiB |
| profile | `.dsh-market/state.json` | 1 MiB |
| home | `settings.yaml` | 4 MiB |
| home | `cordis.patch.yml` | 1 MiB |

Each manifest records present, missing, or explicitly omitted state. Present files record original/stored size, portable mode, SHA-256, and redacted field paths. Validation requires the exact seven paths in order, rejects unknown/traversal paths and unknown manifest fields, caps the manifest at 256 KiB and aggregate content at 41 MiB, and rechecks type, non-reparse ancestry, size, mode, blob inventory, and hash before restore. `profile/package.json` is the required profile identity file: capture and restore both require it to be present and to contain a JSON object, and restore recalculates its plugin count. Any of the other six entries may independently remain `Missing`; malformed optional structured files remain explicit `Omitted` entries.

Healthy publication uses slot-bound `healthy/.next-slot-N` and `healthy/.old-slot-N` names. Before capture or listing, recovery inventories only configured slot names, fully validates every visible/orphan candidate including its manifest, blob hashes, profile, kind, and required package, then applies a deterministic rule: a valid destination wins, otherwise a valid old slot is restored, otherwise the valid next slot is published. Unknown names, reparse points, invalid candidates, and ambiguous external content fail closed. The destination-to-old and next-to-destination rename cut points are both covered by deterministic reconstruction tests.

## Sensitive content boundary

All seven allowed files are parsed as JSON or YAML before capture. `serde_yaml_ng 0.10.0` was selected as the maintained Serde-compatible continuation of archived `serde_yaml`; its fetched dependency closure is locked, and final tests run offline.

Mapping keys are normalized by removing ASCII punctuation and folding case. The explicit protected set is `apiKey`, `apiToken`, `token`, `authToken`, `accessToken`, `refreshToken`, `password`, `passphrase`, `secret`, `secretKey`, `clientSecret`, and `privateKey`. Matching fields are removed from stored content and only their escaped structural paths enter the manifest. Non-string YAML mapping keys, invalid UTF-8, invalid JSON/YAML, excessive protected paths, or unrepresentable paths fail closed: capture records the file as omitted with a fixed non-content reason. The engine does not claim to identify arbitrary unknown business strings or secrets stored under unrelated key names.

`.credentials.yaml`, `.env`, `sessions/`, and every other non-policy path are never enumerated or opened. Tests place dummy secrets in each excluded location and prove none appears under the Nexus data root. `inspect` returns metadata only.

During restore, the current file is parsed again. Both snapshot-recorded protected paths and newly present protected keys are merged from the current file into restored structured content. If a protected value cannot be reinserted because structure changed, or the current structured file cannot be parsed safely, prepare/apply fails without blanking or overwriting it. Original files are renamed into `$DSH_HOME/.nexus-restore/<profile>/<ticket>/`; secret-bearing undo bytes are never copied into the Nexus snapshot or transaction directory. On Windows, private-file ACL strength remains inherited from the user-owned DSH home; snapshot bytes themselves are already redacted.

## Restore transaction boundary

1. `prepare_restore` fully validates the snapshot and all current targets, computes materialization need, and durably emits a serializable ticket/record without changing DSH content. One active restore is allowed per profile; transaction history is bounded to 64 records.
2. The outer Nexus lifecycle journal records its `Prepared` intent, then calls `apply_restore`. Each operation is write-ahead recorded, the old regular file is renamed to the in-home undo directory, and the desired file is durably replaced. Missing snapshot entries mean delete-with-undo; omitted entries mean preserve current content.
3. A crash or injected failure leaves an explicit `Applying` transaction. Store construction performs no automatic restore recovery. An outer `Prepared` decision calls `recover_restore(..., Rollback)`; it reverses completed operations in descending order. An explicit resume decision calls `ResumeApply`. Both paths are idempotent. Rollback validates the optional backup directory against the canonical DSH home before enumeration and again immediately before every backup read or rename; a real Windows junction substituted at that point is rejected.
4. Changes to `package.json`, `pnpm-lock.yaml`, or `pnpm-workspace.yaml` set `materialization_pending`. This crate never runs pnpm. Only the caller's successful dependency install may call `mark_materialized`.
5. The outer journal's `Committed` decision calls `ResumeCommit`/`commit_restore`; commit refuses pending materialization, removes the bounded in-home undo set, and is idempotent. A crash while removing undo files remains `Committing` and can only finish commit, not roll back.

The file system cannot atomically replace all seven files as one operation. Namespace moves now use `MoveFileExW` with `MOVEFILE_WRITE_THROUGH` on Windows; Unix renames are followed by syncing both affected parent directories. Backup cleanup first moves each secret-bearing file to a deterministic tombstone with the same durable primitive, truncates and syncs it to zero bytes, and only then permits the transaction record to advance. A replayed Windows directory deletion can therefore leave only empty remnants, which terminal commit/rollback calls clean idempotently; it cannot resurrect backup secret bytes after `Committed` or `RolledBack`. Tests inject failures immediately after namespace mutation and after record persistence. These are ordering and process-restart tests, not a claim of physical power-cut or storage-hardware acceptance.

Durability otherwise comes from per-file replacement, write-ahead operation states, same-home rename backups, and the caller-owned outer intent decision. Callers must serialize write operations for one profile; this crate does not add a second cross-process lock beside Agent ownership. The repeated path checks close the reviewed junction substitution point under that serialized ownership model; the crate does not claim handle-relative protection against a concurrently privileged attacker swapping directories between checks and operations.

## Verification

- `cargo test --offline -p nexus-snapshots`: exit 0; 17 passed, 0 failed; doc tests 0.
- `cargo clippy --offline -p nexus-snapshots --all-targets -- -D warnings`: exit 0.
- `cargo fmt -p nexus-snapshots -- --check` and `git diff --check`: exit 0.
- Synthetic tests cover required-package rejection, each optional-file missing case, both healthy-publication crash cuts, healthy rotation and manual retention, oversize, corrupted hashes, unknown/traversal manifest paths, a real Windows junction substituted at rollback's backup path, pre-mutation fail-closed parsing, namespace-before-record recovery, deterministic mid-apply rollback/resume, idempotence, materialization gating, and secret exclusion/preservation.
- No real DSH home, credentials, sessions, Harness process, install, or deployment was accessed.
