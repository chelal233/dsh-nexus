# Electron migration acceptance

Electron 44 is the only desktop host; the previous host and compatibility dependencies have been removed.
It is not a published release or a completed four-platform acceptance.

## Boundaries

- React layout, navigation, styling and existing wording remain unchanged.
  The only requested additions are **Update** beside the footer version, hidden
  until a download is ready, and an automatic Launcher update setting.
- Rust `nexus-agent` continues to own business data. `nexus-desktop-bridge`
  reuses `nexus-launcher-core` over private stdio and authenticated Agent HTTP.
  Renderer processes receive neither filesystem APIs nor Agent credentials.
- Launcher and DSH Shell have separate single-instance locks and processes.
  Closing Launcher leaves Shell and services running. Shell resolves its URL
  through Agent, has no preload/Node access, and confines navigation to that
  Harness origin. External links open in the system browser.
- Updater downloads complete packages, disables differential downloads and
  install-on-quit, and asks Agent to shut down only when positively idle.
  Existing lifecycle/update/snapshot/cold-operation gates reject busy work.
  Queued Harness starts are sealed before shutdown. The adapter waits for the
  Agent runtime lock and owned child exit before closing Shell and installing.
  A failed coordination step defers installation rather than killing Agent.
- Update preference is desktop-only. Existing Nexus/Harness paths are not moved.
  When enabled, checks run immediately at startup and every two hours afterward.
  The footer update action installs silently and restarts after Agent coordination.
- Windows Agent creation inherits only its explicit input and log handles.
  Desktop transport handles cannot leak into the independent Agent and prevent
  Launcher from exiting while services keep running.

## Local commands

Run from `apps/nexus-launcher`:

```powershell
pnpm install --frozen-lockfile
node node_modules/electron/install.js
pnpm prepare:agent
pnpm prepare:runtime
pnpm prepare:notices
pnpm prepare:release
pnpm build
pnpm electron:dev
pnpm test:electron
node scripts/electron-smoke.mjs
```

For a local unsigned acceptance installer only:

```powershell
$env:NEXUS_UNSIGNED_SMOKE = '1'
pnpm electron:build
```

Release packaging otherwise requires platform signing; macOS also requires
notarization. Version remains the baseline 0.1.3 for local comparison: do not
publish these artifacts over v0.1.3. A new release needs a new version and build
identity, signed resource inventory verification, and update-feed validation.

For a Windows x64 local release upgrade test, run `pnpm test:electron-update`
after preparing resources and building the frontend. It builds two unsigned NSIS
versions under a unique test app identity, serves them on localhost, and exercises
the real footer action, installation, restart, installed hash and retained setting.
The test removes its isolated installation and retains its report and release
artifacts under `electron-dist/update-e2e-*`. It does not publish a GitHub release.
The two-hour schedule is covered separately with mock timers in `test:electron`.

## Evidence and remaining gates

Windows x64: Rust workspace tests, frontend regressions, desktop policy/update
unit tests, offline bundled runtime, static CRT imports, and real Electron
renderer/Agent smoke have run. The smoke creates temporary Nexus, DSH and Harness
roots and tests the actual sandbox/preload and authenticated Rust transport.
The local unsigned NSIS upgrade from 0.1.3 to 0.1.4-local.1 also passed:
disabled and enabled startup checks, independent Agent survival on desktop exit,
full download, footer installation, automatic restart, package hash comparison,
Agent readiness and update-setting persistence. Two-hour timing uses mock-clock
tests; signed distribution and other platforms still need their own acceptance.

`.github/workflows/build.yml` prepares Windows x64/ARM64 and macOS x64/ARM64
acceptance artifacts. It has not run remotely. The release workflow now consumes the Electron build matrix.

Remaining runtime acceptance: validate independent Shell survival and reconnect with
a real Harness; IME, clipboard/image/file actions; OS login startup/notifications;
signed install, upgrade and rollback on all four platforms; busy-update deferral
and full-download replacement using two signed versions. Check both architectures'
update metadata and SHA-512 assets. No production data or release was changed.

Shared resources, icons and release scripts live in `desktop/`. There is no legacy host fallback.
