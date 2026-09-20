# Changelog

[简体中文](CHANGELOG.md)


## 0.1.8

1. **Use the official Harness Desktop**

   Launch the Desktop included in your selected managed Harness release instead of a Nexus-built replacement client. The Desktop option appears only when both the selected release and your platform support it.

2. **Clearer workbench and tray controls, with easier profile switching**

   Web and Desktop now share one Harness area with clear mode, status and available actions, while profile switching remains easy to find. The tray adds Desktop launch, separate Web/Desktop stop controls, profile and maintenance shortcuts, and distinct options to exit only the launcher or stop all services.

3. **A practical recovery path when startup fails**

   Nexus distinguishes failing plugins, missing services and plugins still waiting for dependencies, then recommends repairs based on observed evidence. With your confirmation, it can temporarily disable the relevant plugins and check and start again. Packages and data remain available, and plugins can be re-enabled later.

4. **More reliable startup results**

   A running process or accessible webpage no longer counts as successful startup on its own. Nexus checks client plugins and core services, showing checking, limited functionality, startup failure or unverified states, including when loading remains blocked. These checks cover startup, not every conversation, tool or operation during use.

5. **Fewer duplicate files and less first-start preparation**

   Nexus and official Desktop share a matching Electron runtime, with Desktop dependencies prepared in advance and verified local files reused on later launches. Preparation shows its stage, elapsed time and a cancel action, so you can follow progress or stop waiting. It does not download missing dependencies during preparation.

6. **More dependable offline packages and portable directory moves**

   Full exports include matching Harness files and required runtimes, allowing import on the same OS and architecture without downloading dependencies. This release fixes stale paths after moving a portable directory and preserves Unix executable permissions and relative links. Configuration/data-only exports remain partial packages and do not replace a full offline package.

7. **Linux ARM64 downloads and broader platform delivery**

   AppImage, DEB and RPM packages join the existing Windows and macOS installers and archives. All five targets passed CI and package checks. Windows ARM64 and Linux ARM64 currently offer Web Harness; official Desktop appears only where both upstream Harness and Electron support it. Package checks do not establish compatibility with every Linux distribution or business workflow.

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
