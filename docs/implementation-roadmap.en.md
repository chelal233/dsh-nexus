# Implementation status and next steps

[简体中文](implementation-roadmap.md)


## Implemented capabilities

Electron and browser entry points, bundled runtimes, Harness installation/linking, release slots, authenticated readiness, configuration repair guidance, recovery checkpoints, bundled plugins/notifications, GitHub updates and portable packages exist in source. See [architecture](architecture-baseline.en.md) and [limitations](known-limitations.en.md). This does not replace per-release device acceptance.

## Current working tree, not yet released

- Rebind bundled runtimes after relocating a portable directory.
- Apply saved runtime settings at the next Harness start without affecting the running instance.
- Prevent stale refresh results from replacing newly saved settings; persist Agent log level.
- Bilingual documentation and actual UI screenshots.

Validate against the [acceptance checklist](acceptance.en.md), then record the actual release version in the changelog.

## Future decisions

Extend bundled plugins and public interfaces for concrete needs. A standalone window remains optional; full upstream Desktop parity is not promised. Users choose their market; Nexus does not host a registry. Launcher version switching cannot guarantee compatibility between upstream data formats.

The [original plan](history/baselines/implementation-roadmap.md) is historical evidence, not current completion status.
