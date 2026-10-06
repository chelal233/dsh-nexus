# Troubleshooting

[简体中文](troubleshooting.md)


Record the Nexus version, build ID, OS, and architecture, and retain the original error. These steps do not require deleting Harness user data.

| Symptom | Check and response |
| --- | --- |
| `Resolve the blocking startup checks…` | Inspect the actual `blocked` items; this is a summary, not the cause |
| `configured_path_missing` or missing Node launch program | Compare next launch inputs with the current extracted location; inspect `resources/runtime` and stale pins |
| `ENOENT … app-update.yml` | Use a complete release package with the file under `resources`, not a copied EXE. The inspected v0.1.7 Windows x64 ZIP and installer both contain it |
| Saved runtime still launches an old path | Known in v0.1.7 and fixed in v0.1.8; use a complete updated package. A workaround must check both runtime settings and the advanced launch program |
| JSON configuration parse failure | Identify the file and line/column, preserve a backup, and use guided repair or restore a valid configuration; do not reset all directories |
| Plugin Loader failure | Preserve the stack, run compatibility checks, and check plugin provenance/declarations and Nexus/Harness versions; do not assume every failure is a user plugin defect |
| Interrupted operation awaiting recovery/cleanup | Retry recovery/cancellation through the interface; do not delete markers to bypass process ownership checks |
| Missing verified rollback target | Complete authenticated readiness for the current version, or follow an explicitly offered manual confirmation path; a version pointer is not health evidence |
| No notifications | Check categories, conditions, observer connection, OS permissions and do-not-disturb; terminal delivery requires its listener to remain open |

## Portable relocation

v0.1.7 can retain an automatically selected bundled runtime as an old absolute path. v0.1.8 re-resolves `bundled` selections against the current installation and updates the linked launch program. Explicit external paths remain untouched. If an old save marked the selection external, it requires an explicit correction rather than guessing from the directory name.

## Diagnostics and recovery

Launcher startup diagnostics may remain available without the Agent. Blocking startup checks prevent Harness startup; repair the reported issue, recheck, then start explicitly. Legacy records without recoverable process identity may require one computer reboot as directed. Ordinary path configuration errors should not require rebooting the computer.

Report a minimal reproduction, original error, version, build ID, and redacted screenshots. Inspect diagnostics before sharing; do not publish tokens, API keys, private conversations, or full configurations. See [security reporting](../SECURITY.en.md).

## Nexus diagnostic thresholds

- **Blocking**: this startup failed. Show the category, original evidence, remedy, and applicable Nexus settings, profile, plugin or log assistance.
- **Limited functionality**: upstream explicitly reports optional activation warnings and the current process passes authenticated Web checks. Final availability still requires current-client startup acceptance; plugins are not automatically disabled.
- **Informational**: version declarations and unrelated fallback links are not startup failures.
- **Unconfirmed cause**: preserve the failure without assigning blame or recommending arbitrary plugin isolation.

Last reviewed on 2026-10-05 against the latest published Harness release [dsh-v0.2.1-alpha.1](https://github.com/deepseek-ai/deepseek-harness/releases/tag/dsh-v0.2.1-alpha.1) (prerelease), pinned to commit `5badb15009ae1756c3afe0ae0cef1faafc290ccc`. Nexus v1.0.6 checker v17 rules were compared with current app-boot, profile/dependency resolution, Loader, CLI and Desktop startup source. See [startup coverage](startup-failure-coverage.en.md#latest-release-review-and-acceptance-gaps) for pinned sources and changed behavior. Unknown formats preserve their original text, and older-version compatibility rules remain.

Source inspection and simulated regressions do not accept real upgrades, full session migration or every plugin on the latest release. Upstream may skip unreadable or incompatible bundles; a configured plugin name is not proof of loading. The bundled Desktop offline runtime remains locked to `0.1.6-alpha.2`; that shipped lock is maintained separately from this diagnostic review baseline.

Installing dependencies, editing patches, switching versions and disabling plugins require explicit user actions. Nexus diagnostic suggestions do not execute these repairs or terminate unrelated processes. Native loading may normalize manifests or remove retired bundles. Independent checks change only an isolated copy and display changed files; managed CLI startup saves a manifest/patch recovery point and refuses launch if it cannot be saved. Restore through Maintenance → configuration repair; restoration does not start Harness. Editing and prelaunch backups remain limited to 32 KiB per file. Inspection and recovery allow up to 256 KiB per file to accommodate native formatting expansion; replacement still backs up current contents and checks their fingerprint. Large files have a read-only preview of at most 32 KiB. The truncated preview cannot be saved, but recovery points remain available. This is not a full-data snapshot. Skipped bundles retain their names and confirmed first reason lines. Unclassified continuation text marks the evidence incomplete; inspect the full log. Overall readiness does not accept these bundles' features. For a managed CLI with the verified official readiness contract, its current-run skip summary survives log-tail growth. Old-run records and post-ready skipped-bundle output are excluded from that summary.

## Desktop, preparation and blocked plugins

- No Desktop option: use the selected managed Harness resources and platform checks. The reviewed upstream Desktop targets are Windows x64 and macOS x64/ARM64; Windows ARM64 and Linux x64/ARM64 use Web. Six-platform Nexus packages do not establish official Harness Desktop availability on every target.
- Long preparation: inspect the stage and elapsed time; cancel if needed. Retry after checking local resources and disk space. Missing or mismatched offline resources require a matching complete Nexus/package, not an online dependency install.
- Waiting services: locate the provider using declarations and original errors. A waiting consumer is not automatically the faulty plugin. Disable only a relevant plugin after reviewing the evidence, then recheck and retry.
- Failed offline import: check OS/architecture, package completeness and integrity. Do not treat a configuration-only export as a full environment.
- Desktop opens but a model/tool fails: use the official client error and logs; Nexus does not diagnose every runtime business error.

See [startup coverage](startup-failure-coverage.en.md) and [Desktop](harness-desktop.en.md).
