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

## Platform parity follow-up

Windows and macOS share update, login startup, tray, system-browser, notification
preferences and third-party marketplace selection flows. macOS now also has native
Terminal.app sessions with pinned runtime/profile and durable terminal leases,
Profile file opening, Finder recovery-artifact reveal, menu-less Command editing
shortcuts, and a BEL fallback for Apple Terminal notifications.

Harnesses on Unix start in their own process group, receive SIGTERM for graceful
stop, and have remaining group members signalled before the leader is reaped.
macOS automatic Web readiness checks listener process-group ownership with the
system lsof, in addition to current-session credential evidence. macOS live log
reclamation uses block-aligned F_PUNCHHOLE without truncation; unsupported volumes
report failure and retain the file. API contract: Apple's
[fcntl manual](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/fcntl.2).

Local Windows regression: Agent 292 + main 1 and core 131 tests passed (6 ignored),
plus 23 Electron tests. The Unix child module and its tests passed an isolated
macOS ARM64 target typecheck. Full cross-check stopped at the missing native C
cross-compiler; these results are not native Mac acceptance. Mac CI must compile
and run the added graceful-stop, descendant cleanup, terminal handshake and sparse
retention tests. Both Mac architectures still require real Terminal/Finder,
notification, startup and signed-update acceptance before release.

## Desktop compatibility completion

Compatibility checks use disposable HOME/profile copies under the Nexus
`compatibility/work` directory. They cache only a verification result and never
publish a generated profile. Real runs and desktop package operations use the
selected native profile and its original DSH_HOME. Explicit plugin disable/enable
edits the native manifest atomically, preserving packages and restoration order;
the disabled list and its migration version live in the same document.

Legacy generated profiles carrying validated ownership markers are moved intact
to `DSH_HOME/.nexus-retired-profiles` after a successful, quiescent check. The UI
provides an archive entry. A selected legacy copy requires explicit source
selection rather than silently discarding its edits. Old references remain
resolvable through archived markers. Ordinary user profiles named `nexus-*`
without those markers are untouched. Interrupted checks reconcile process
ownership before reclaiming temporary directories, including pre-reservation
orphans; legacy pending records remain supported.

The independent window now rediscovers the current Harness URL and credential,
shows a local recovery page on service loss/renderer failure, and bounds client
loader readiness to 45 seconds. Retry can restart Agent startup preparation;
opening Launcher remains available during failure. Profile switches run under
Agent lifecycle ownership and leave a durable completion/interruption status.
An interrupted switch is reported, never replayed automatically.

Launcher and the independent window both consume notification events. Shared
event claims prevent duplicate banners. The client bridge reports visible,
focused conversation identity with a ten-second lease; only the matching task
page suppresses unfocused-only task reminders. Launcher focus applies only to
management notices. Clicking a task notice
opens its session through the built-in client plugin. If neither native host is
running, native banners are not delivered. Notices include a bounded conversation
title and visible reply, question, approval reason, failure or job detail; reasoning
blocks and raw tool results are excluded. Previews are stored in the local bounded
notification snapshot. Missing fields fall back to status text, never another turn.

`plugins/nexus-desktop-compat` implements the public profile/package services;
`plugins/nexus-desktop-bridge` provides native directory selection, window
geometry and session navigation. The preload exposes the upstream file-path
bridge for native File objects. All capabilities use a sandboxed renderer and
main-frame/origin-checked IPC. Runtime imports from the upstream desktop package
and private APIs are outside this compatibility contract.

The selected third-party market remains optional. Its ordinary CLI path accepts
the sources supported by the user's Harness (including GitHub); Nexus does not
require all plugins to use the exact-npm compatibility entry. Compatibility and
Canary probes load the same Host services but reject package writes and profile
switches. Package operations retain ownership until subprocess-tree cleanup.

Offline acceptance to perform on Windows and both Mac architectures:

- Keep only the independent window open; complete/answer/approve tasks and verify
  banner categories, focus conditions, deduplication and click-to-session behavior.
- Stop/restart Harness, change its port, kill its renderer/Agent, and interrupt a
  profile switch; verify recovery/retry and preserved configuration.
- Choose self-managed or the third-party market; install/remove local test packages,
  cancel an installation, kill during download, then reopen and inspect/retry.
- Drag a real file, select/cancel a native directory, use clipboard/IME and native
  terminal; verify file paths, pinned runtime and terminal-lease cleanup.
- Test signed packaged updates, download interruption/newer-tag replacement and
  notifications/OS permissions on each actual machine. Local automated checks
  do not establish these platform acceptance results.

Completion evidence (Windows, build `desktop-parity-test-20260916-02`): full Rust
workspace tests passed; 118 UI tests and 33 Electron tests passed. The Electron
suite included real Cordis and the published dshmarket 1.38.1 package-operation
adapter, with controlled subprocess outcomes (not a live registry installation).
The real Electron smoke passed Launcher startup, independent recovery-window
Agent access, renderer sandboxing and rejection of native calls from a foreign
document. Release binaries passed the static Windows runtime import check and
the resource manifest verified 2,651 files. Native Mac acceptance remains open.
