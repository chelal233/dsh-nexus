> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../../README.md) · [当前文档 / Current documentation](../../../README.md)

# P0 recovery backend report

Date: 2026-09-05

## Scope and identity

- Worktree: `E:/git/dsh-nexus-phases/p0-integration`
- Branch: `codex/nexus-p0/integration`
- Fixed starting HEAD: `4615f0e2e4422e7209f76152156080b24622af53`
- This change extends the existing recovery transaction. It does not replace
  the outer `CheckpointRestoreIntent`, snapshot ticket, Prepared/Committed
  recovery, or materialization flow already present at the starting HEAD.
- No real DSH home, Harness log, plugin command, installation, network, GUI,
  deployment, or production data was used.

## Native profiles and plugins

`GET /v1/profiles` retains `active_profile` and the legacy `profiles` string
array, now sourced from valid ordinary directories below the resolved
`DSH_HOME/profiles`. `manifests` adds each profile's ordered
`dsh.profile.bundles` and dependency inventory. Bundle entries are built-in
and non-removable; remaining `dependencies` are removable. Symlinked,
oversized, malformed, and escaping manifests cannot become selectable.

`POST /v1/profiles` `select` requires a manifest-backed profile and the shared
positive stopped/unowned Harness gate. It persists selection and never starts
Harness. Creation and deletion remain outside P0.

`plugin_remove` accepts only the selected profile and a package currently
marked removable. It holds lifecycle and updater gates in a detached owner,
uses the registered current release's verified `apps/cli/lib/bin.js`, and runs
the exact official argument order:

```text
<pinned-node> <verified-cli> plugin --profile NAME remove PACKAGE
```

The child receives the shared pinned runtime environment and explicit
`DSH_HOME`, runs in the canonical target profile, and uses the existing DSH
owned-process implementation (Windows kill-on-close Job Object; Unix process
group). Exit code plus bounded stdout/stderr are returned without converting a
non-zero CLI exit into success. No shell or caller-supplied executable/path is
accepted. `nexusctl profile remove NAME PACKAGE` exposes the same Agent action.

## Snapshot content and recovery status

Snapshot `detail` and explicit `inspect` now return text only from the engine's
fixed seven-file stored allowlist. The engine first validates the whole
snapshot, then rechecks each returned blob's stored size and SHA-256 at the
point of read. Missing and omitted records return no content; protected values
remain absent because content comes from the already sanitized stored blob.
Each file is capped at 64 KiB and the aggregate content at 256 KiB. Truncation
has an explicit note; existing omitted reasons remain visible. Inventory list
inspection stays metadata-only so listing snapshots cannot multiply content
responses.

`GET /v1/recovery` always advertises an explicit manual entry. It returns the
current Harness observation, whether stop is required, bounded/redacted tails
from only the current Nexus-owned log session, fatal-prefix observation as
advisory metadata, startup error, diagnostic read errors, and pending restore
status. It does not read DSH_HOME logs and does not claim the fatal heuristic is
an authoritative failure decision. Shared launcher-core and Tauri route gates
allow GET only. `nexusctl recovery` exposes the read-only response.

## Synthetic verification

- `cargo test --offline -p nexus-protocol -p nexus-core -p nexus-snapshots -p nexus-agent -p nexus-cli -p nexus-launcher-core`: PASS. Agent 118, core 30,
  snapshots 17, launcher-core 11, protocol 13; all other selected unit/doc test
  targets passed with zero failures.
- `cargo check --offline --locked --manifest-path apps/nexus-launcher/src-tauri/Cargo.toml`: PASS. The nested lockfile gained only the already-required
  snapshot crate and its YAML dependencies; unrelated package versions remain
  pinned.
- `git diff --check`: PASS.

Focused coverage includes ordered bundles, built-in/removable classification,
invalid profile rejection, built-in removal rejection, exact CLI argv,
explicit DSH_HOME and profile cwd, non-zero exit/error preservation, fixed
seven-file content, secret omission, response truncation, route method gates,
and bounded/redacted fatal diagnostics. Existing checkpoint cancellation,
Prepared/Committed startup recovery, transient materialization retry, abort,
healthy rotation, and manual retention tests also remained green.

## Follow-up blocker fixes

- Recovery now applies the same bounded diagnostics redaction to both
  `startup_error` and embedded `harness.error`, and the complete response
  serialization regression confirms the synthetic bearer secret is absent.
- Recovery log reading caps the opened reader at 16 KiB plus one sentinel byte;
  oversized metadata and bytes observed after the metadata check both report
  truncation without unbounded `read_to_end`.
- Plugin stdout/stderr reading caps the opened reader at 64 KiB plus one
  sentinel byte before applying the existing truncation annotation.
- `cargo test -p nexus-agent`: 122 passed; `git diff --check`: passed.

## Follow-up cold publication gate fix

`ReleaseAction::Register` now validates its request, acquires the shared
updater mutation gate, and only then checks pending checkpoint/publication
state and registers the release. This matches the cold publication ownership
boundary without changing cold transaction or owner APIs. A synthetic cold
owner fixture holds the updater gate and confirms registration is rejected
without creating the requested slot.

- `cargo test -p nexus-agent`: PASS after the follow-up.
- `git diff --check`: PASS.

## Follow-up recovery sentinel fix

The recovery tail helper now removes the `limit + 1` sentinel before decoding
or redaction, then caps the already redacted response at the 16 KiB limit on a
UTF-8 character boundary. The truncation flag remains derived from the
metadata/read sentinel, including newline-free concurrent growth. Focused
coverage includes 16,385 newline-free bytes, multibyte UTF-8, and invalid
UTF-8 payloads.

## Real isolated plugin acceptance

A local file-only test package was installed offline into an isolated native profile. Real official CLI removal exposed the same Node verbatim-script-path incompatibility; a shared node_script_argument boundary now serves both pnpm and the verified built CLI while canonical identity remains unchanged. Removing the final dependency also showed pnpm omits the dependencies key. Read-only native inventory now treats an absent dependencies object as empty while rejecting an explicitly invalid value. Both focused regressions pass. The real repeated removal returned removed=true, exit_code=0, refreshed inventory successfully, and the selected profile was returned to web. No user profile or external package was modified.
