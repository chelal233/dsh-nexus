> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../../README.md) · [当前文档 / Current documentation](../../../README.md)

# P0 Runtime Foundation Phase Report

- Task: `nexus-p0`
- Phase: `runtime-foundation-expanded`
- Status: foundation implementation complete; the independent-review deadline finding is resolved on the `runtime-fix1` isolation branch
- Base: `f91f28d81a9ae72f88772f67e3ed23679462971b`
- Branch: `codex/nexus-p0/runtime-foundation`
- Worktree: `E:\git\dsh-nexus-phases\p0-runtime-foundation`
- Review fix branch/base: `codex/nexus-p0/runtime-fix1` at
  `8261a6e35c485a85c7b4e203c682bb956d52e62a`

## Outcome

This phase establishes the runtime configuration, requirements, observation,
planning, and child-process contract needed by the later provisioning and cold
installation phase. It does not download, install, execute Corepack, start
Harness, change system configuration, or claim that the remaining P0 work is
complete.

- `config.json.runtime` is optional and backward compatible. It stores pinned
  absolute Node, pnpm, and Git paths with `system|nexus` ownership, an
  `official|npmmirror` source (default `official`), and a
  `portable|system` mode (default `portable`). Structural validation does not
  require pinned files to exist, so a removed runtime does not prevent Settings
  from loading the config for repair. Nexus-owned pins must be lexically below
  the single `NexusPaths.runtimes_dir` root.
- `ConfigStore::transaction` uses one process-wide mutex for atomic
  load -> mutate -> validate -> replace across separately constructed store
  instances. Agent Harness/update/runtime config writes and updater switch-ref
  writes use this transaction. Harness readiness URL preservation remains
  inside the same transaction.
- The pure requirements module reads both `package.json` and
  `apps/cli/package.json` from the registered release root. Each manifest uses
  a bounded `take(MAX + 1)` read and a post-read limit check. The fail-closed
  parser covers the release's required npm range forms (`^`, `>=`, `||`, and
  exact versions), exact `pnpm@VERSION`, boundary cases, and explicit
  prerelease matching.
- `POST /v1/runtime/plan` accepts only a registered `release_id` plus source and
  mode preferences. It returns release-derived requirements, stable ordered
  tool classifications (`reusable|missing|incompatible|unverifiable`), reasons,
  ownership, and suggested actions. `plan_id` is the complete deterministic
  serialized plan fact set with a `runtime-plan-v1:` prefix. It is deliberately
  not described as a security hash; a future installer must recompute and
  compare it before acting.
- `GET /v1/runtime` remains read-only. Configured pins are canonicalized and
  version-probed through the existing bounded observation path. A missing,
  unsafe, or invalid pin reports an explicit reason and never falls back to a
  different PATH candidate.
- Both runtime handlers create one six-second absolute deadline before config or
  registered-release filesystem work. Config loading, release-root lookup,
  bounded manifest reads, observation, child probes, and cleanup consume that
  unchanged deadline. Every synchronous filesystem operation shares the same
  process-wide three-permit owner. A timed-out blocking operation keeps its
  permit until its real call returns, so detached work is capped and later
  requests still return under their own deadline.
- `SetRuntime` and `ClearRuntime` share the Agent config route and retain the
  existing lifecycle-lock-then-try-update-gate order, including lifecycle wait
  and update-conflict behavior. Protocol and CLI literals, launcher-core
  validation, and the Tauri route allowlist are synchronized. `/v1/runtime`
  remains GET-only; `/v1/runtime/plan` is POST-only.

## Downstream runtime contract

All install, build, launch, terminal, plugin, and profile-materialization
consumers should load the same `RuntimeConfig` and call these `nexus-core`
symbols rather than rebuilding pin or environment logic:

- `RuntimeConfig::{pin,validate_for_paths}` and `NexusPaths::runtimes_dir`
- `resolve_runtime_command` -> `RuntimeCommandSpec { program, prefix_args }`
- `build_runtime_child_env` -> child-only PATH entries
- `build_pnpm_args` -> process-local `--config.minimumReleaseAge=0` and the
  registry selected by `RuntimeSource`
- `runtime_requirements::{load_runtime_requirements,node_version_satisfies,package_manager_version_matches}`

`resolve_runtime_command` represents a pnpm `.js`, `.cjs`, or `.mjs` pin as the
pinned Node program plus the pnpm script in `prefix_args`. This supports a
verified Corepack cache entry without executing a Corepack shim. The planner
does not currently scan an unconfigured Corepack cache; the provisioning lane
must locate and validate a candidate before it writes that explicit pin.

The stable public protocol coordinates are:

- `nexus_protocol::RuntimeConfigPayload`, `RuntimePinPayload`,
  `RuntimeOwnership`, `RuntimeSource`, and `RuntimeInstallMode`
- `nexus_protocol::RuntimePlanRequest` and `RuntimePlanResponse`
- `nexus_protocol::ConfigAction::{SetRuntime,ClearRuntime}`
- Agent routes `GET /v1/runtime`, `POST /v1/runtime/plan`, and
  `GET|POST /v1/config`

## Test-first evidence

The three new behavior groups were exercised red before implementation:

- Runtime requirement fixtures compiled and failed three requirement/range
  assertions before the parser was implemented.
- The cross-instance config transaction regression compiled and deterministically
  lost the runtime field with per-instance gates before the shared transaction
  gate was added.
- The plan fixture compiled and failed on an empty `plan_id` before deterministic
  serialization and classification were implemented.

The independent review fix added a fourth behavior group. The GET regression
compiled and failed because the `ConfigFile` blocking hook was never entered
within 200 ms, proving that `ConfigStore::load` was outside the shared owner.
After moving the deadline to the outer handlers, the regression passed together
with blocked requirements, three saturated preparation permits plus a later
request, and preparation-time-not-reset-before-child/cleanup cases.

During final verification, changing the command fixture to the real cached
shape `pnpm/bin/pnpm.mjs` exposed one stale test expectation for its parent PATH
directory. The targeted test was rerun after correcting that assertion and
passed; no production behavior changed for this diagnosis.

## Verification

- `cargo test --offline -p nexus-core shared_runtime_command_and_child_environment_cover_pnpm_js_entries -- --nocapture`
  - Exit code: 0
  - Result: 1 passed
- `cargo test --offline -p nexus-core -p nexus-agent -p nexus-protocol -p nexus-launcher-core`
  - Exit code: 0
  - Result: nexus-core 30, nexus-agent 97, nexus-protocol 13,
    nexus-launcher-core 11; all associated doc-tests passed
- `cargo check --offline -p nexus-cli`
  - Exit code: 0
- `cargo test --offline -p nexus-agent`
  - Exit code: 0
  - Fix1 result: 101 passed; all associated doc-tests passed
- `git diff --check`
  - Exit code: 0

## Changed paths

- `.agent-memory/PROJECT_STATUS.md`
- `apps/nexus-launcher/src-tauri/src/main.rs`
- `artifacts/takeover/p0-runtime-foundation-report.md`
- `crates/nexus-agent/src/lib.rs`
- `crates/nexus-agent/src/runtime.rs`
- `crates/nexus-agent/src/runtime_plan.rs`
- `crates/nexus-agent/src/supervisor.rs`
- `crates/nexus-agent/src/updater.rs`
- `crates/nexus-cli/src/main.rs`
- `crates/nexus-core/src/lib.rs`
- `crates/nexus-core/src/runtime_requirements.rs`
- `crates/nexus-launcher-core/src/lib.rs`
- `crates/nexus-protocol/src/lib.rs`
- `docs/architecture-baseline.md`

## Open acceptance and later work

- Implement confirmed portable/system provisioning and cold install/build using
  the shared command, args, and child environment contract.
- Recompute and compare the deterministic plan before any install action.
- Wire every runtime consumer to the same pins; do not copy command construction
  into individual consumers.
- Perform real cached pnpm 11.7.0 reuse, cold install, built CLI launch, plugin
  reconciliation, profile materialization, GUI/Tauri, and Unix acceptance in
  their authorized phases.
- This phase did not rebuild Tauri resources or run Tauri/GUI acceptance; it
  changed only the shared validator route and matching Tauri allowlist/tests.
