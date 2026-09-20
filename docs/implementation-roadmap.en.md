# Implementation status and next steps

[简体中文](implementation-roadmap.md)


## Implemented capabilities

Electron and browser entry points, bundled runtimes, Harness installation/linking, release slots, authenticated readiness, configuration repair guidance, recovery checkpoints, bundled plugins/notifications, GitHub updates and portable packages exist in source. See [architecture](architecture-baseline.en.md) and [limitations](known-limitations.en.md). This does not replace per-release device acceptance.

## Shipped in v0.1.8

Official Harness Desktop, a unified workbench and maintained tray, evidence-based startup repair, shared offline runtimes, portable path/save fixes, and Linux ARM64 AppImage/DEB/RPM are released. All five targets passed CI/package checks. Bilingual guides and screenshots now describe this release. See the [changelog](../CHANGELOG.en.md) for user-facing details and [acceptance](acceptance.en.md) for remaining device/workflow checks.

## Future decisions

Extend bundled plugins and public interfaces for concrete needs. Reuse official upstream Desktop within the Harness/Electron support intersection rather than maintaining a replacement client. Users choose their market; Nexus does not host a registry. Launcher version switching cannot guarantee compatibility between upstream data formats.

The [original plan](history/baselines/implementation-roadmap.md) is historical evidence, not current completion status.
