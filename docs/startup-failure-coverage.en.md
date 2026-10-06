# Nexus startup failure scope and coverage

[简体中文](startup-failure-coverage.md)

Last reviewed: 2026-10-05, against the latest published Harness release [dsh-v0.2.1-alpha.1](https://github.com/deepseek-ai/deepseek-harness/releases/tag/dsh-v0.2.1-alpha.1) (prerelease, published 2026-10-03), pinned to commit `5badb15009ae1756c3afe0ae0cef1faafc290ccc`. The local implementation is Nexus v1.0.6 checker v17.

This review compares current startup, profile-loading, CLI and Desktop source with existing classifiers and regression cases. Source inspection and simulated regressions do not accept a real upgrade, full session migration or every third-party plugin on this Harness release.

The bundled Desktop offline runtime remains locked to Harness `0.1.6-alpha.2`. That is the shipped runtime, not this document's latest diagnostic review baseline. This work changes neither the lock nor the user's selected Harness.

## Responsibility and success

Nexus covers preflight, process launch, plugin loading, service dependencies, connection establishment and Web client startup acceptance. After startup it observes process exits and connection state; model requests, tools, conversations and approvals belong to Harness. Official Desktop exposes its internal readiness through its own client; a running Desktop process is not equivalent to Web client readiness.

MCP initialization that blocks startup is in scope; a later individual invocation failure is not. Arbitrary `error` or `timeout` lines in runtime logs cannot establish startup failure.

Web success requires current configuration checks, a reachable current process, and a client-ready report matching that run. A process, HTTP response, or historical success alone is insufficient. Unknown states remain unverified. Handling includes accurate explanations and manual next steps, not necessarily configuration changes.

## Latest-release review and acceptance gaps

| Current upstream behavior | Identification or acceptance boundary |
| --- | --- |
| Required failures use grouped `startup failed`, `Failed plugins` and `Plugins waiting for services` output; optional failures retain per-entry warnings | v17 retains these formats and distinguishes failed packages from waiting services. A waiting consumer is not automatically a faulty provider |
| `loadProfileDirectory` collects unreadable or incompatible bundles in `skippedBundles`; `reportSkippedBundles` emits their reasons | v17 bounds native log records, preserving names, reasons and truncation. Authenticated readiness remains limited, while independent fatal errors still block. Current logs must match the process identity; an unverified client cannot be called ready. Canary remains inconclusive when bundles were skipped, rather than certifying unloaded plugins |
| Native loading uses `normalizeShippedProfile` and `dropRetiredBundles` to normalize managed bundle lists and remove retired bundles, writing the manifest when applicable | Independent checks run in an isolated copy, recording changed files and before/after SHA256 without publishing changes. Managed CLI startup backs up the original `package.json` and `cordis.patch.yml`, including absence, for Maintenance → configuration repair. Backup failure blocks launch; Nexus does not create a first-launch profile. Automated and managed-process integration checks validate this protection, not a full real upgrade on every platform |
| Runtime invariant plugins and `./invariant` exports are removed; plugin subpath metadata resolution changes | Generic missing-module/export classification preserves the importer. Per-plugin imports and subpath declarations still need review; do not automatically recreate removed APIs |
| Unpackaged Desktop still requires a valid target and `DSH_DESKTOP_PRIMARY_RUNTIME_DIR` | Target, prepared resources and environment are launch inputs. Inspect Nexus preparation and the original error instead of disabling arbitrary user plugins. Real preparation and launch on this release were not accepted in this review |
| Desktop Host starts with `--port 0` and supplies the actual authenticated URL over IPC | A fixed port cannot establish official Desktop readiness. Process creation, child readiness and window/client state require separate evidence |
| Web adds `--public-url`, including external addresses and path prefixes | Tracked from the release notes; reverse proxies and custom public URLs were not accepted here. A public URL is not local authentication or current-client readiness evidence |

Pinned sources: [startup policy and grouped diagnostics](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/packages/boot/app-boot/src/index.ts), [profiles and skipped bundles](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/packages/boot/app-boot/src/profile.ts), [dependency resolution](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/packages/boot/app-boot/src/profile-resolution/resolver.ts), [Loader patch targets](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/vendor/loader/src/config/tree.ts), [CLI arguments](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/cli/src/args.ts), [Desktop targets](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/desktop/scripts/desktop-build-paths.mjs), [Desktop preparation](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/desktop/scripts/development-project.ts), [Desktop main](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/desktop/src/main.ts), [Desktop Host port and readiness](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/desktop-host/src/index.ts). See the linked release notes for breaking changes and the Web public URL.

## Coverage

Native-observation regressions cover skipped bundles after authenticated readiness, separate fatal errors, cache reuse, noisy output and bounded multilingual evidence. Managed CLI launches with the verified official readiness contract preload a bounded observer before bundle imports. The official `appReady` commit seals its summary in the current host record; subsequent log growth cannot erase it, and post-ready skipped-bundle output is excluded. Agent readers validate the run, owned Host PID (including verified descendants) and schema bounds; the UI also requires the current generation and authenticated client evidence. Unknown startup contracts retain ordinary log observation. Configuration protection covers isolated-copy changes, an actual managed test-process launch, exact restoration and backup failure. Editing and prelaunch backups retain the 32 KiB per-file limit. Inspection and recovery have a separate 256 KiB bound for manifests expanded by native formatting; replacement still backs up current contents and verifies their fingerprint. Files beyond the recovery bound are not overwritten. This protects configuration files, not the entire session database, and never restores automatically. Multiline native reasons retain only their confirmed first line; unclassified continuation text marks evidence incomplete and is not attributed to a bundle.

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

## Keeping the review current

- On each review of a new official Release, record the date, tag, prerelease status and pinned commit. Label the latest prerelease and stable channel separately; a moving `master` is not a release baseline.
- Compare startup output, profile/bundle resolution, CLI, Desktop preparation and readiness protocols. Update this page, both troubleshooting translations and the classifier's source comment.
- Record unchanged rules, required code adaptations and untested behavior separately. New classifiers need corresponding regressions; source review is not real-upgrade acceptance.
- Retain compatibility rules for older output. Track the bundled runtime lock and historical acceptance separately so they do not replace the latest release review.

## Implementation references

- Preflight: `crates/nexus-agent/src/preflight.rs`
- Startup probe/classification: `crates/nexus-agent/src/compatibility.mjs`
- Error signatures and remedy categories: `crates/nexus-agent/src/startup-diagnosis.mjs`
- Process supervision: `crates/nexus-agent/src/supervisor.rs`
- Client startup observation: `apps/nexus-launcher/electron/client-audit.mjs`, `plugins/nexus-desktop-bridge/client.js`
- Repair UI/execution: `apps/nexus-launcher/src/views/startup.tsx`, `startup-repair.tsx`
- Regression coverage: `crates/nexus-agent/tests/compatibility.test.mjs` and Launcher startup UI tests
