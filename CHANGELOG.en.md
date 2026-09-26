# Changelog


## 1.0.3 — 2026-09-26

- Fix bundled macOS/Linux Node bin/npm, bin/npx and available Corepack entry points. They now launch from the correct package directory after packaging dereferences symbolic links, preventing npm_probe_failed and the resulting unavailable pnpm status when bin/node is selected. Old build caches are regenerated; installed applications need an update to receive the fix.
- Open Desktop can restore an existing background desktop window without restarting its tasks. The running host advertises support; older independent hosts retain their existing controls.
- Clarify that closing a window may leave Desktop running. Stop Desktop before switching modes or changing data. Compatibility with older and newer Harness interfaces remains; user Harness installations are not upgraded automatically.

## 1.0.2 — 2026-09-24

- Protect official runtime files from package-manager traversal of source links during Desktop plugin installation. Detach official source projections in the Profile while preserving configuration and external plugin links.
- Open legacy settings.yaml or the new Profile configuration according to the selected Harness settings interface, including compiled artifacts; report unknown layouts explicitly.
- Restore current secrets by unique entry id rather than array position, preventing credential misassignment after configuration reordering. Reject missing or ambiguous identities before writing.
- Add regression coverage for V3/V4 live session notifications, migrated configuration recovery and plugin write boundaries.

## 1.0.1 — 2026-09-23

- Adopt the transparent Whale Station Master icon across the window, tray, packages and sidebar.
- Move diagnostic bundles beside local dependencies and offline profile repair. Bundles and offline repair are collapsible without clearing form state.
- Remove duplicate plugin management embedded in Maintenance, while preserving Built-in plugins and Configuration and plugins.
- Fix plugin repository links that did not open in Electron. A restricted desktop bridge opens HTTPS GitHub links and rejects other schemes, domains and credential-bearing URLs.
- Move Native integration above Release identity and Launch configuration explained below Help.
- Add `_portable.zip` to Windows/macOS ZIP attachment names, preserving installer names. Update collection, checksum and provenance checks accordingly. The filename does not imply storing user data beside the executable.

## 1.0.0 — 2026-09-23

### User-visible changes and fixes

- **Manage and repair profiles while Harness is stopped.** Plugin listing, enabling, disabling, installation, checks and removal use the selected Harness version's official manager and protection rules where available. Newly installed plugins remain disabled until enabled; disabling retains dependencies. Older versions retain a compatibility path.
- **Run first and diagnose failures.** Normal startup avoids repeating profile copies and compatibility checks. Only evidence of missing dependencies permits one bounded repair and retry. A changed profile, release or stop request cancels stale recovery work.
- **Preserve separate timeout and failure evidence.** A timeout means readiness is unverified; it does not stop or restart a running Harness. A later real failure still receives a separate diagnostic record.
- **Fix stale official packages shadowing Desktop modules.** Desktop binds official modules from the selected release and backs up older official packages that shadow them, including incompatible settings providers. Third-party plugins and profile settings are preserved.
- **Make offline repair controlled and recoverable.** Repairs provide previews, backups, rechecks and recovery paths. Waiting service consumers are not all disabled as a substitute for finding the provider failure. Node, pnpm and Git use an explicit configured path first and bundled tools otherwise; the proposed system-environment priority was cancelled.
- **Switch prepared versions directly.** A completed preparation offers a switch-to-version action. Harness 0.1.7 compatibility and startup acceleration follow capabilities and declared build targets rather than a version allowlist.
- **Clarify profiles and history.** Snapshots and checkpoints show local-time timestamps in drawer details, with readable and source views. Plugin versions, repository links, problem markers and removal entries are included.
- **Report settings failures.** Random port 0 and related preference forwarding are fixed. Tool-path override scope is documented; log-level and OS startup-setting failures surface their reasons.

### Offline tools and distribution materials

- Bundled Git includes its command line, SSH and Git LFS, but excludes the optional GCM browser-login helper and its dependencies. System and user Git configuration are not changed. Users needing another login helper can explicitly select their own configured Git.
- Runtime tools remain bundled with Nexus. Corresponding source for Git and related components accompanies installers as a separate release attachment and is not loaded during normal use. Redistributing binaries must also satisfy the applicable source-delivery conditions.
- Publication checks original-notice hashes, the source inventory, source attachments and installer provenance. Incomplete materials prevent publication.

### Known limits and acceptance scope

- Nexus handles startup, readiness and offline repair; it does not guarantee capture of every third-party runtime exception. The historical session-manager JSON error was not reproduced with an isolated official Desktop Host: the relevant APIs returned HTTP 200 and valid JSON. This does not establish that the original error is fixed or that upstream is at fault.
- Official Desktop is enabled only where both upstream and Electron support it. A Nexus package for a platform does not imply that Harness provides Desktop there.
- Switching to an older Harness slot does not downgrade upstream session data formats. Keep a pre-upgrade data backup.
- Automated tests, real subprocess tests, isolated Windows installer upgrades and five-platform CI are reported separately. Skips are not passes. CI package smoke tests do not replace device acceptance for every distribution, IME, display or complete workflow.


## 0.1.10

### Problems fixed

1. **A pending or failed check no longer opens a broken page.** Previously a running Harness process and URL could enable the workbench or tray browser action even while the client was unavailable. Workbench, tray, notification links and automatic opening now require a successful client check for the current run. Opening re-reads the latest status to reject stale enabled buttons. Stop, diagnosis and repair remain available while checking, unverified or blocked; a process alone is not readiness.
2. **Tray launches report their outcome.** Native Windows balloons show starting, ready, failed or unverified states and open the workbench when clicked. Results are deduplicated per launch, and success from an old run cannot announce a new launch as ready. Stop and cancellation end the corresponding feedback. A user has accepted the balloons on a real Windows machine.
3. **More consistent stop, cancel and restart.** Web tray restart uses the workbench flow. Startup checks can be cancelled, with cancellation bound to the displayed operation so stale menus cannot cancel new work. Desktop workbench and tray share restart behavior: a failed stop never starts another instance, and another Stop during restart cancels its pending launch.
4. **Actionable Desktop startup errors.** Web and Desktop share classification for missing dependencies, damaged configuration, incompatible interfaces and waiting services. Official details remain available with dependency inspection, profile management and log entry points. Guidance names the independent `desktop` profile and does not blame a plugin merely because it is waiting for a service.

5. **Brief Windows state-file locks no longer immediately fail startup.** Desktop state writes retry transient sharing conflicts for at most about 0.5 seconds while retaining atomic replacement. They never delete the prior state and still report persistent errors.

### New and improved mechanisms

1. **Preview and confirm offline dependency repairs.** Maintenance checks missing links against the selected Harness lockfile and local package identity, including pnpm 11 shortened directory names. It only restores missing links to verified packages, without downloading or replacing existing files or broken links. Before changes it records the lockfile, relevant manifests and plan; afterwards it journals and rechecks results. Stale previews require a new inspection. The affected mode still needs a startup check after repair.
2. **Less cache waiting without skipping verification.** Desktop runtime cache checks use at most 16 concurrent filesystem requests while inspecting every entry and link boundary and rebuilding damaged caches. A read-only comparison of 8,909 entries on the same machine reduced this stage from about 1.5–1.8 seconds to 0.5–0.7 seconds, with identical inventory results. This is not a total cold-start measurement or a guarantee for every machine.
3. **See where startup time is spent.** Web shows input checks, compatibility checks and process creation; Desktop shows preparation stages. Timings freeze on completion and reset for a new launch, excluding subsequent usage time.
4. **Read changes before downloading an update.** The update confirmation dialog adds View release notes beside the version, opening that version's GitHub page. Download consent, progress, validation, deferred restart and explicit installation remain in place. An isolated Windows update test covered interrupted-download recovery and post-restart version and file-hash checks.

### Known issues and compatibility boundaries

- **Upstream Desktop marketplace package management remains unresolved.** Installing, updating or restarting through dshmarket inside official Desktop can still fail. This release does not spoof PATH, services or profile ownership, nor force a Web fallback; an upstream fix is still needed.
- **Dependency repair has a defined scope.** It restores verified missing links within Nexus-managed Harness versions. External sources, existing broken links and missing package files can require other recovery. It does not reinstall every dependency or prove every plugin starts.
- **Startup checks are not comprehensive runtime monitoring.** Unverified is not a confirmed failure; passing checks does not validate every conversation, tool or third-party plugin. First-run preparation still depends on storage, configuration and plugin count.
- **The OS can suppress notifications.** Windows notification settings or Do Not Disturb may hide balloons. Workbench and tray retain the status. User acceptance on Windows does not establish real-device notification acceptance on macOS or Linux.
- **Platform scope and offline behavior are unchanged.** Official Desktop remains available on Windows x64 and macOS Intel/Apple Silicon; Windows ARM64 and Linux ARM64 provide Web mode. Linux ARM64 offers AppImage, DEB and RPM without claiming real-device acceptance on every distribution. Nexus includes its runtimes; preparation after acquiring Harness or importing a full offline package on the same OS and architecture needs no network. Downloading third-party plugins still requires a connection.

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
