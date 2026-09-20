# Build and release engineering

[简体中文](github-release.md)


The current Electron matrix covers Windows x64/ARM64, macOS x64/ARM64 and Linux ARM64. Supported targets must be supported by both Harness and Electron.

## Two workflows

`Desktop build`: frontend/Rust/Electron checks, target-native helpers and runtimes, notices and resource manifests, packaging, installed/extracted smoke checks, then artifact collection.

`Publish release`: runs for `v*` tags and requires `v<package version>`. It reuses a complete successful five-target build of the same commit, or runs the build matrix. It verifies artifacts, signs the checksum manifest, creates a draft, uploads assets, and publishes a prerelease only after success.

Pushing source, creating a tag, passing builds, and publishing a Release are distinct states. Do not blindly rerun draft creation when one already exists. Do not move published tags or attach artifacts from a different commit.

## Release-note standard

Every version must update `CHANGELOG.md`, `CHANGELOG.en.md` and `.github/RELEASE_TEMPLATE.md`, with matching Chinese and English content on GitHub. Use the v0.1.8 notes as the level-of-detail example; do not force the same number of items on smaller releases.

- Group changes by user-facing capability or problem. Give each core item a clear benefit-led title and explain what changed in use, which problem it solves, and any relevant action or limitation.
- Cover meaningful feature additions, experience improvements, fixes, offline behavior and platform differences without repeating the same change in several items. Small releases can have fewer items, but still explain the user impact.
- Do not substitute commit messages, dependency versions, refactors, API names or build-tool details for a release note. Mention implementation details only when they explain a user benefit, compatibility requirement or migration step.
- Keep both languages equal in meaning and detail, including limitations and upgrade instructions. Do not publish one full language and a shortened translation of the other.
- Describe only changes included since the previous published version. Support quantitative claims with evidence, distinguish CI/package checks from real-device acceptance, and do not promise untested compatibility.
- Review the final GitHub body before publication: no placeholders, stale version entries, duplicate content or internal paths. Documentation corrections to a published release may update its body without moving its tag or replacing verified binaries.

## Before publishing

1. Align Nexus versions in root Cargo.toml, Cargo.lock, and Launcher package.json; update both changelogs.
2. Build natively on each matching target; verify lockfiles, runtime digests, and notices.
3. Complete workflow tests and install/extract checks; record real user-interaction acceptance separately.
4. Check installer/DMG, ZIP, architecture update YAML, build metadata, and per-target checksums. Confirm `resources/app-update.yml` exists in packages.
5. Confirm all five targets succeeded for the same commit, then push the version tag. Inspect the publish run and Release rather than treating build success as publication.

## Local packaging

Stage resources using the [development guide](../apps/nexus-launcher/README.en.md), then run in the Launcher directory:

```sh
pnpm electron:build
```

Outputs are in `apps/nexus-launcher/electron-dist`. Explicitly set `NEXUS_UNSIGNED_SMOKE=1` only for unsigned local acceptance. The full Windows x64 gate is `pnpm release:gate`; clear stale `NEXUS_BUILD_ID` and `CARGO_BUILD_TARGET` first. Do not upload the entire gate log directory as release assets.

## Assets and verification

Names follow `dsh-nexus_<version>_<windows|macos|linux>_<x64|arm64>.<exe|dmg|zip|AppImage|deb|rpm>`. The current complete matrix produces 26 build assets plus the aggregate checksum manifest, signature, and certificate: 29 Release assets. See [security](../SECURITY.en.md) for signature verification.

Windows: `Get-FileHash <file> -Algorithm SHA256`. macOS: `shasum -a 256 -c <architecture>_SHA256SUMS.txt`. `.github/scripts/verify-release-assets.mjs` verifies versions, commits, targets, and hashes.

## Interrupted uploads

Keep the draft. Compare existing assets' status, size, and SHA-256; upload only missing files from that same build, preserving correct assets. Publish only after verification. Manual completion does not turn a cancelled/failed Actions record green; disclose the distinction.

Record CI compilation, installed smoke, OS signing trust, update acceptance, and business workflows separately. v0.1.7 passed five-target builds and was published after interrupted uploads were completed; this is not a claim of exhaustive real-device acceptance.
