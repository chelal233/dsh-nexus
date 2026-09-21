# Changelog

[简体中文](CHANGELOG.md)


## 0.1.9

### Problems fixed

1. **Official Desktop opens visibly, and tray actions match the workbench.** Fix Windows launches where the process started without a visible window. Tray start, stop, open-page and terminal actions share the workbench flow, including checks, recovery feedback and status refresh. The workbench adds Desktop Close and Restart; a failed stop never starts a second instance.
2. **Waiting is no longer mistaken for failure.** Request time budgets accommodate background preparation and compatibility checks. After a transport timeout, Nexus checks actual state before repeating a launch or suggesting plugin repair. “Ready with warnings” identifies the affected optional plugins and their reported reasons.
3. **Desktop startup errors surface in Nexus.** Configuration or plugin-loading failures bring the launcher forward and expand diagnostics, once per failed operation. Failure details survive process exit. Official error reports are read even if structured evidence is missing or malformed, preserving the first cause so users can investigate missing service providers instead of blaming every waiting plugin.
4. **Web opens automatically only after client checks pass.** Blocking failures stay in the launcher with their reason and diagnostic entry point, instead of opening a broken page before offering recovery. Nexus does not automatically disable plugins.

### New and improved mechanisms

1. **Less repeated preparation for Web and Desktop.** For verified Harness 0.1.6-alpha.2, normal Web startup observes the actual managed process and browser client instead of starting an isolated probe and then launching again. Stale evidence cannot mark a new process ready. Manual checks, profile/version-switch checks and unsupported versions retain isolated checks. Bounded parallel dependency copying and fewer repeated scans/archive reads reduce preparation while retaining integrity and link checks. One same-machine, same-profile Web comparison improved from about 67 to 10.4 seconds; this was not a post-reboot cold-cache test or a guarantee for every machine.
2. **Desktop has startup checks too.** Nexus observes the actual official client rather than treating a running process as success. Unsupported checks or long waits remain “Unverified”, and official recovery remains available on failure. These checks cover startup, not every subsequent application error.
3. **Clearer profile and terminal scope.** Web shows the selected profile; Desktop uses its independent `desktop` profile, without automatically synchronizing plugin settings. The DSH terminal opening banner lists its bound profile, working directory, data home, Harness source and Desktop profile directory, and explains dsh versus npm/pnpm targeting. It is an opening snapshot; reopen the terminal after switching profiles. Expanded profiles list plugins, snapshots, then saved checkpoints.
4. **Updates download only after confirmation.** Finding a version only announces availability. Clicking Update opens a confirmation dialog; confirming starts the download with progress. Only a verified download offers Restart later or Update and restart. Download completion never installs automatically. Save work before applying an update: it stops Harness and restarts Nexus.
5. **Fresh marketplace installations default to dshmarket 1.52.0.** Upstream fixes official Desktop profile recognition and some request-origin checks, reducing accidental targeting of Web profiles. This is a first-install default, not an automatic replacement of existing installations, and it does not resolve the Desktop package-management limitation below.

### Known issues and compatibility boundaries

- **Marketplace install, update or restart inside official Desktop can still fail.** Although dshmarket 1.52.0 recognizes the Desktop profile, its package operations do not fully use the environment provided by official Desktop. Upgrading the marketplace does not guarantee that dsh/pnpm or restart errors disappear. Nexus does not spoof profile ownership, force a Web fallback or automatically modify upstream plugins; an upstream fix is still needed.
- **Third-party command-output encoding is not a general fix shipped in this release.** A decoding adjustment was verified on one machine during investigation, but is not included in Nexus packages and can be overwritten by a plugin update. Existing damaged Harness dependency links likewise are not automatically restored just by upgrading Nexus; this release does not claim to repair every dependency failure.
- **Startup checking is not comprehensive runtime monitoring.** “Unverified” does not mean a confirmed failure, and passing startup does not validate every conversation, tool or plugin feature. First-run preparation still depends on storage, plugin count and configuration.
- **Platform scope stays within upstream support.** Official Desktop is available on Windows x64 and macOS Intel/Apple Silicon. Windows ARM64 and Linux ARM64 provide Web mode. Linux ARM64 offers AppImage, DEB and RPM, without claiming device acceptance on every Kylin, UOS or other distribution.
- **The offline contract is unchanged.** Required runtimes ship with Nexus. After acquiring Harness, Desktop preparation and full offline-package import on the same OS and architecture need no network. Third-party plugin downloads still require a connection; configuration/data-only packages cannot replace full offline packages.


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
