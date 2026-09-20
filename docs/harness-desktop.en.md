# Official Harness Desktop

[简体中文](harness-desktop.md)

Nexus launches the official Desktop included in the selected managed Harness release. It no longer maintains a replacement client. The CLI restriction on Desktop-only profiles still applies. Stop Web before choosing Desktop in Workbench. Nexus verifies local resources, invokes the release's preparation script, and starts the official window without a development debugging port or Nexus Web compatibility plugins. The old `--nexus-shell` entry redirects to official Desktop.

## Offline delivery

- Users acquire Harness; Nexus ships Electron, Python, Node and the pinned Python dependencies. Desktop preparation does not download dependencies.
- Full offline exports include matching Desktop resources. Older portable Node/pnpm selections are supplemented from matching bundled Nexus resources; export fails clearly if those resources are unavailable.
- Import installs no online dependencies. It prefers packaged runtimes and recreates temporary links and run directories from local files.
- Missing, corrupt or mismatched resources require a matching complete Nexus installation or offline package. There is no silent runtime switch or online fallback.
- Configuration/data-only exports remain partial packages and cannot replace a complete offline environment.

## Shared runtime and preparation

A normal installation keeps one Electron runtime. Desktop starts as a separate process using the same executable and a small bootstrap that sets official application paths, name, version and source-run mode before importing upstream `lib/main.js`. It starts no Nexus window, Agent or replacement client and does not modify Harness source. macOS retains the signed Nexus application structure.

The Launcher Electron version exactly matches the official runtime pin, currently 44.0.0. Packaging checks declared, installed and pinned versions; upgrades must validate them together. First preparation verifies and extracts preassembled local resources and creates official dependency links. Later starts reuse files when metadata and link targets are unchanged; corrupt or missing files rebuild from verified local resources. Preparation runs in a cancellable child process, with stage and elapsed-time feedback.

Full exports also carry a matching `host.tar.gz` containing application binaries and bootstrap resources, not user data. A matching receiving Nexus shares its host; otherwise the packaged host is extracted, so import does not depend on a coincidentally installed version. Export verifies Desktop bootstrap support and application digests. Legacy schema 1/2 packages retain independent Electron fallback. Full transfer requires the same OS and architecture; mismatches are rejected. Unix permissions and relative links are preserved. macOS exports the complete signed .app and checks its signature before export and after extraction; Windows uses a bounded host-file set.

CI prepares base runtimes, Desktop offline resources and native checks, then release manifests, packages and installed-package checks. It probes shared Electron and extracts/checks preassembled dependencies. Missing or invalid resources fail the build rather than deferring downloads to first launch.

Web compatibility checks cache a parallel file inventory, excluding generated official Desktop directories. Plugin, configuration, environment, file metadata or link-target changes invalidate it. Manual checks still force the probe; incomplete inventories fall back to actual checks.

## Supported targets and evidence

The current upstream runtime supports Windows x64 and macOS x64/ARM64. Its lock does not declare Windows ARM64 or Linux ARM64 Desktop. Those Nexus packages retain verified unsupported markers, offer Web, and hide Desktop actions; they do not substitute x64 binaries for ARM64.

The current compatibility baseline is Harness 0.1.6-alpha.2 with Electron 44.0.0 and its locked runtimes. Other releases must pass artifact and lock compatibility checks. Windows local regressions and WSL x64 Unix offline round-trip/process-group checks were exercised. v0.1.8 passed CI/package checks on all five targets, including macOS signed-app/offline-host checks and Linux ARM64 package smoke. This is not complete Harness business acceptance on every device.

Linux ARM64 provides the Nexus graphical launcher and Web through AppImage, DEB and RPM. Its Agent builds against glibc 2.28 with ABI checks. Package formats do not prove compatibility with every distribution. Support is limited to the intersection of Harness and Electron.

## Status, configuration and tray

Nexus observes backend startup, client plugin activation and UI mounting in the actual official Desktop instance; it does not launch a second instance for verification. Status distinguishes checking, ready, failed and unverified. Configuration or activation failures retain the original error and official recovery UI. Unsupported observation or timeout remains unverified, without claiming success or killing the process. Observation stops after startup; runtime business errors remain the official client’s responsibility. Closing Launcher does not close Desktop. Close Desktop before version/configuration changes or data export; conflicting writes are blocked.

The tray separates Browser and Official Desktop submenus with launch/open/stop actions, and groups configuration, maintenance and exit separately. Desktop appears only when the managed Harness and platform support it. Shared versions and data make Web/Desktop launch and configuration writes mutually exclusive. Preparing, running and stopping restrict conflicting actions. The profile shortcut opens the configuration page; it does not switch profiles immediately.

Exit launcher (keep Harness running) preserves Harness. Stop all services and exit stops Desktop first, then asks Agent to stop Web and itself; exit completes only after successful stops.

On macOS, signing changes embedded binaries. The release flow refreshes resource digests after inner signing, then reseals the outer app; notarization runs only when enabled. Installed checks use post-signing digests. Ad-hoc verification is not Apple notarization.

The Desktop stop action asks the worker that launched the instance to stop its owned process tree; preparation can also be cancelled. Instances launched by older Launchers may need their official window closed manually. A failed stop keeps Launcher open and displays the error.

Web uses the profile selected in Nexus; official Desktop uses `profiles/desktop`. Switching modes does not copy or overwrite profiles, and editing Web plugins does not change Desktop. Workbench shows the actual profile for each mode.

Initial Web checks avoid a duplicate full dependency scan and move stopped probe directories to background cleanup. Cache reuse still checks configuration, dependencies and links. Desktop reuses the inventory made during extraction, verifying final links without a second complete traversal.

Normal Web startup on verified Harness 0.1.6-alpha.2 observes the actual instance: it waits for official appReady, binds the evidence to the current run and Windows Job / Unix process group, then verifies the current Web endpoint and client. A delegated Host must belong to that same tree. Missing evidence cannot grant readiness. Unknown versions, custom launch commands and independent compatibility checks retain the existing flow; optimization never disables plugins automatically.

## Third-party market profile selection

Local inspection found that dshmarket 1.39.0 recognizes the `desktopProfiles` service from a different Desktop implementation. Official Harness 0.1.6-alpha.2 provides neither that service nor a `--profile` argument in its Host process, so the market falls back to `web` for installed listings and package operations. Its installed badge therefore does not prove that Desktop loads the plugin. Official Desktop still reads `profiles/desktop/package.json`; inspect that profile in Nexus. Until the market supports official Desktop, manage plugins through an entry point explicitly targeting desktop. Nexus does not rewrite user plugins or emulate another host contract.
