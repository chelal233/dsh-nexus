# Changelog

[简体中文](CHANGELOG.md)


## Unreleased

- Rebind bundled runtimes after relocating a portable directory.
- Use saved runtime settings on the next Harness start without affecting the running instance.
- Prevent stale refreshes from replacing newly saved settings; persist Agent log level.
- Restructure Chinese and English user, developer and maintenance documentation with actual UI screenshots.

These changes are in the working tree and are not included in the releases below.

## 0.1.7

- Fix macOS disk-space-check compilation.
- Select an executable PowerShell explicitly in Windows regression tests instead of relying on legacy system paths on ARM64.

## 0.1.6

- Fix frontend formatting checks and restore platform build/release pipelines.

## 0.1.5

- Improve layouts, localization and repair guidance for damaged configuration.
- Prepare upstream versions before manual switching, retaining the current Harness selection.
- Isolate compatibility checks from user profiles; check local plugin version declarations without network requests.
- Use Nexus confirmation windows; block conflicting operations while retaining startup cancellation.
- Bound declaration-report size and rebuild oversized legacy caches.
- Publish portable Windows ZIP packages and improve recovery/data protection.

## 0.1.4

- Complete the Electron migration and remove Tauri; target Windows x64/ARM64 and macOS Intel/Apple Silicon.
- Drive updates from GitHub Release tags; check at startup and every two hours when automatic updates are enabled.
- Show background download progress at the lower left, then apply and restart through Update after verification. Settings support disabling automatic checks and checking manually.
- Reject incomplete/corrupt downloads; recheck the release source on restart and validate cached downloads against the latest version.
- Add ownership-based recovery after forced termination; fix desktop exit hanging on inherited Windows Agent handles.
- Prepare bundled Node/npm/pnpm per target architecture and validate digests, versions and architecture.
- Collect third-party notices and cover them with resource hashes.
- Create prerelease drafts only after all tag builds succeed, with build information and SHA-256 manifests.

### Review fixes

- Lock protection-record read/modify/write operations across processes to prevent lost Harness-home and external-directory entries.
- Bound directory traversal depth in capacity and external-directory fingerprint checks; report excessive nesting explicitly.
- Wait for failed Harness start/stop process trees outside the Supervisor lock, keeping diagnostics responsive.
- Move idempotency receipt I/O to the blocking pool; use actual volume budgets (`statvfs`) outside Windows.
- Provide recovery guidance for unrecognized snapshot entries; avoid following reparse points when opening snapshot files.
- Reject unknown protocol fields and device-namespace paths such as invalid `DSH_HOME` values.
- Sort frontend idempotency fingerprints by code units and share one pending-request client.
- Pass CLI `update confirm` operation IDs and tokens separately; increase timing-test budgets to tolerate load.
- Complete the three-step setup guide with “Check and start”.

0.1.7 is available as a prerelease. Subsequent releases must document actual changes, platform acceptance and limitations. Build and installation checks generate release attachments; local verification does not establish acceptance on other platforms.
