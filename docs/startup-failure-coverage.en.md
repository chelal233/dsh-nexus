# Nexus startup failure scope and coverage

[简体中文](startup-failure-coverage.md)

Baseline: Harness dsh-v0.1.6-alpha.2 and Nexus v0.1.8 checker v13. This records implemented coverage, not automatic repair for all future releases or third-party defects.

## Responsibility and success

Nexus covers preflight, process launch, plugin loading, service dependencies, connection establishment and Web client startup acceptance. After startup it observes process exits and connection state; model requests, tools, conversations and approvals belong to Harness. Official Desktop exposes its internal readiness through its own client; a running Desktop process is not equivalent to Web client readiness.

MCP initialization that blocks startup is in scope; a later individual invocation failure is not. Arbitrary `error` or `timeout` lines in runtime logs cannot establish startup failure.

Web success requires current configuration checks, a reachable current process, and a client-ready report matching that run. A process, HTTP response, or historical success alone is insufficient. Unknown states remain unverified. Handling includes accurate explanations and manual next steps, not necessarily configuration changes.

## Coverage

| Stage or condition | Identification and response | Acceptance boundary |
| --- | --- | --- |
| Runtime, entry, version, path | Locate missing prerequisites; link to versions/runtime settings | Recheck after repair; custom commands only within verifiable capabilities |
| Configuration, arguments, profile | Distinguish parse/read errors, reserved names and invalid arguments | Configuration/patch errors open relevant files; port/argument errors lead to settings |
| Data paths and permissions | Preserve read/write, denied-access and locking evidence | User resolves the named path; no automatic elevation or data deletion |
| Disk full | Recognize startup ENOSPC separately | Free the affected volume and recheck; never delete sessions automatically |
| Port conflict | Preflight and EADDRINUSE evidence | Change port or address a confirmed owner; do not guess and kill unrelated programs |
| Dependencies and interfaces | Missing modules, declarations, exports, layout, patch targets and duplicates | Repair the relevant dependency/version/patch; declarations are not loading evidence |
| Plugin loading | Separate failed items from waiting items; map to current-profile packages | Suggest isolation only with evidence, then recheck |
| Official plugins waiting for services | Remains a failure even with no disable candidate | Find providers, not blame consumers; static replacement evidence is not a complete graph |
| Nexus integration failure | Identify known bundled-component failures separately | Repair/update Nexus and inspect logs, not arbitrary third-party isolation |
| Early exit | Preserve exit code and available root-cause output | Exit alone cannot identify a faulty plugin |
| Startup timeout | Recognize probe timeout and retain output tail | Prefer specific causes; component timeouts are not overall readiness timeouts; no infinite retry |
| Probe stop timeout | Distinct from readiness timeout | Confirm the previous process stopped before rechecking |
| Configuration changed during checks | Invalidate the report and request recheck | Do not suggest mutations from stale evidence |
| Optional activation warnings | Follow upstream startup policy; show usable but limited state | Required services still waiting cannot pass merely because HTTP responds |
| Client load failure, blocking or absent report | Distinguish checking, unverified, missing services, blocked plugins and ready | Require a report for the current run; absence cannot identify a failed plugin |
| Unfinished recovery/install transaction | Block conflicting startup and link to recovery/maintenance | Finish or cancel through existing recovery before rechecking |
| Unknown output or new errors | Preserve original text and log entry | Manual diagnosis or updated adapter; do not claim repaired |

## Regression coverage and limits

- Accept upstream line-by-line warnings and grouped fatal startup reports, preserving package attribution and waiting services without inventing missing attribution.
- Required official services still waiting do not pass merely because no plugin can be disabled.
- Prefer underlying causes over configuration wrappers or waiting symptoms; permission, port and disk errors do not generate plugin-disable suggestions.
- Explain Nexus integration failures, disk full, changed inputs, cleanup timeout and silent exit separately.
- Preserve diagnostic tails on timeout; ordinary component `timed out` output does not directly match overall startup readiness timeout.
- Route port, permission and argument repairs to the relevant UI.
- Automated regressions use temporary configurations and controlled startup processes. They do not modify user plugins or induce real disk exhaustion, file destruction or production failure.

Classification uses verified upstream output and limited structured reports, not an exhaustive parser for every exception. Provider attribution relies on declarations and explicit replacement evidence; service names cannot reconstruct arbitrary third-party graphs. Corruption, permissions, network failures and third-party defects may require manual repair. Startup success does not verify every model, conversation or tool, and Nexus does not read business conversations to expand diagnostic scope.

## Implementation references

- Preflight: `crates/nexus-agent/src/preflight.rs`
- Startup probe/classification: `crates/nexus-agent/src/compatibility.mjs`
- Process supervision: `crates/nexus-agent/src/supervisor.rs`
- Client startup observation: `apps/nexus-launcher/electron/client-audit.mjs`, `plugins/nexus-desktop-bridge/client.js`
- Repair UI/execution: `apps/nexus-launcher/src/views/startup.tsx`, `startup-repair.tsx`
- Regression coverage: `crates/nexus-agent/tests/compatibility.test.mjs` and Launcher startup UI tests
