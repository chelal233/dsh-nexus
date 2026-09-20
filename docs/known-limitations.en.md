# Known limitations and current status

[简体中文](known-limitations.md)


This page tracks user-visible boundaries rather than historical reviewer batches. Original records remain in the [archive](history/baselines/known-limitations.md).

- v0.1.7 may retain old bundled paths after portable relocation and override saved runtime settings with an old launch path. These are fixed in v0.1.8; upgrade affected packages and still check explicit external paths.
- Declaration compatibility does not guarantee plugin behavior or data-format compatibility. Downgrading Harness may hide sessions using newer formats.
- v0.1.8 Web mode checks client plugins after host readiness by keeping a hidden page in the same instance while Launcher runs. Missing bridge reports, load failures and timeouts remain unverified. The real Electron path with a controlled plugin tree has passed locally; each Harness release still needs separate acceptance. Plugin activation does not verify all conversations, tools or UI interactions.
- Browsers cannot directly expose every Electron native interface. Full compatibility with third-party private Desktop APIs is not promised.
- Nexus bundles runtime dependencies for supported releases. Building external sources or adding third-party native dependencies may still require your own compiler tools.
- Incomplete downloads are not installable updates. Portable packages require more than the EXE. End-to-end updates require build-specific acceptance.
- v0.1.8 passed CI and package checks on Windows x64/ARM64, macOS x64/ARM64, and Linux ARM64. This does not establish device acceptance for every distribution, IME, terminal, or permission combination.
- OS code signing, Apple notarization, and checksum-manifest signatures are separate mechanisms; consult artifact metadata.
- Harness/plugins are not a security sandbox. Snapshots are not complete backups of all user data.

New entries should name affected versions, triggers, workarounds, fix versions, and validation level. Do not mark unverified conclusions resolved.

## Platform and offline boundaries

Official Desktop currently supports Windows x64 and macOS x64/ARM64 with a compatible managed Harness release. Windows ARM64 and Linux ARM64 offer Web only. Linux ARM64 has AppImage, DEB and RPM packages; package format is not a guarantee for every distribution. Full offline transfer requires matching OS/architecture; partial exports are insufficient. Desktop process startup is not proof of internal plugin or business readiness. See [Desktop](harness-desktop.en.md).

## Pending fixes after v0.1.8

The published Windows build can start official Desktop with a hidden window. Current source removes the GUI hide flag, aligns tray Web actions with the workbench, and requires update-download confirmation. These changes are not in the existing v0.1.8 GitHub assets; a local fixed build must be identified by its build ID.


Unreleased changes add Desktop startup observation and accurate profile labels. Supported upstream interfaces can confirm client activation during startup; unsupported observation or timeout remains unverified. Runtime business correctness is outside this check. In single Windows local runs, Web without the Nexus check cache took about 67 seconds from click to client readiness; Desktop with an empty runtime cache and isolated profile took about 38.6 seconds. Profiles differed and OS file caches were not cleared, so this is not a controlled comparison. The 67-second result is the previous optimization baseline. Current source removes the normal-start isolated probe for verified Harness 0.1.6-alpha.2; one same-machine, same-profile run reached client readiness in about 10.4 seconds. This does not characterize every plugin combination or OS cold-cache state.
