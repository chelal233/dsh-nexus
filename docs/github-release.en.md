# Build and release engineering

[简体中文](github-release.md)


The current Electron matrix is Windows x64/ARM64 and macOS x64/ARM64. There is no Tauri, WebView2 installer, x86 package, or fifth target.

## Two workflows

`Desktop build`: frontend/Rust/Electron checks, target-native helpers and runtimes, notices and resource manifests, packaging, installed/extracted smoke checks, then artifact collection.

`Publish release`: runs for `v*` tags and requires `v<package version>`. It reuses a complete successful four-target build of the same commit, or runs the build matrix. It verifies artifacts, signs the checksum manifest, creates a draft, uploads assets, and publishes a prerelease only after success.

Pushing source, creating a tag, passing builds, and publishing a Release are distinct states. Do not blindly rerun draft creation when one already exists. Do not move published tags or attach artifacts from a different commit.

## Before publishing

1. Align Nexus versions in root Cargo.toml, Cargo.lock, and Launcher package.json; update both changelogs.
2. Build natively on each matching target; verify lockfiles, runtime digests, and notices.
3. Complete workflow tests and install/extract checks; record real user-interaction acceptance separately.
4. Check installer/DMG, ZIP, architecture update YAML, build metadata, and per-target checksums. Confirm `resources/app-update.yml` exists in packages.
5. Confirm all four targets succeeded for the same commit, then push the version tag. Inspect the publish run and Release rather than treating build success as publication.

## Local packaging

Stage resources using the [development guide](../apps/nexus-launcher/README.en.md), then run in the Launcher directory:

```sh
pnpm electron:build
```

Outputs are in `apps/nexus-launcher/electron-dist`. Explicitly set `NEXUS_UNSIGNED_SMOKE=1` only for unsigned local acceptance. The full Windows x64 gate is `pnpm release:gate`; clear stale `NEXUS_BUILD_ID` and `CARGO_BUILD_TARGET` first. Do not upload the entire gate log directory as release assets.

## Assets and verification

Names follow `dsh-nexus_<version>_<windows|macos>_<x64|arm64>.<exe|dmg|zip>`. The current complete matrix produces 20 build assets plus the aggregate checksum manifest, signature, and certificate: 23 Release assets. See [security](../SECURITY.en.md) for signature verification.

Windows: `Get-FileHash <file> -Algorithm SHA256`. macOS: `shasum -a 256 -c <architecture>_SHA256SUMS.txt`. `.github/scripts/verify-release-assets.mjs` verifies versions, commits, targets, and hashes.

## Interrupted uploads

Keep the draft. Compare existing assets' status, size, and SHA-256; upload only missing files from that same build, preserving correct assets. Publish only after verification. Manual completion does not turn a cancelled/failed Actions record green; disclose the distinction.

Record CI compilation, installed smoke, OS signing trust, update acceptance, and business workflows separately. v0.1.7 passed four-target builds and was published after interrupted uploads were completed; this is not a claim of exhaustive real-device acceptance.
