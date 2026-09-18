> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../README.md) · [当前文档 / Current documentation](../../README.md)

# Native Tauri Launcher phase report

Task: `nexus-native-launcher`
Phase: `tauri-gui`
Base: `8e794f24b6e1478b30dadd7a543076cceeb79d3c`
Worktree: `<WORKSPACE>/dsh-nexus-tauri`
Branch: `codex/nexus-native-launcher/tauri-gui`

## Worktree handshake

`worktree_verified PASS` was established before edits. The initial checks
reported the declared branch, the base HEAD above, and a clean worktree. The
root Cargo manifest, root Cargo.lock, Agent source, Harness/Desktop source,
and generated target output were kept outside the write scope.

## Delivered

- Replaced the browser Console product entry with a React, TypeScript, Vite,
  Tauri 2 native operator shell under `apps/nexus-launcher/`.
- Added accessible left module navigation and a responsive information
  workspace for Overview, Harness, Profiles, Checkpoints, Updates,
  Diagnostics, and Settings.
- Added loading, empty, error, and degraded endpoint states. Refreshes query
  `/launcher/status`, `/launcher/harness`, and the Agent v1 endpoints through
  Tauri Rust commands. Endpoint failures are retained and shown instead of
  silently becoming empty data. Refresh calls are de-duplicated so an older
  poll cannot overwrite a newer action result.
- Added the System, Light, and Dark theme selector. The selected mode is
  persisted with `localStorage`, System follows `matchMedia` changes, light
  native controls use `color-scheme: light`, and reduced-motion CSS is
  available.
- Added a fixed-route Rust loopback proxy with no browser CORS dependency,
  disabled redirects, per-route GET/POST rules, typed action allowlists for
  GUI mutations, a 32 KiB request cap, and a 512 KiB response cap.
- Added validated loopback Harness URL handling, masked token metadata, a
  `no-referrer` sandboxed iframe, and a system-browser fallback through the
  validated Launcher API. The Windows opener passes one canonical URL to
  `explorer.exe` and never invokes `cmd.exe`; the `&calc.exe` regression case
  is rejected by tests.
- Added Tauri single-instance, notification, tray, close-to-tray, and global
  shortcut integration. Tray left click restores the window without opening a
  conflicting menu. Shortcut registration failure is logged and leaves the
  tray and window usable.
- Added helper discovery from `NEXUS_LAUNCHER_BIN`, a sibling helper, and
  debug-only nearby target directories. The helper starts as
  `nexus-launcher api --no-open --console-port <port>` and failure is visible
  with an actionable message.
- Changed the Rust headless Launcher so `api`, no arguments, and the legacy
  `console` alias expose only the loopback API. Static ServeDir fallback and
  automatic browser opening were removed while Agent and Harness supervision
  routes remain available.
- Deleted all tracked files in `apps/nexus-console/` and migrated the
  architecture baseline to the native GUI and headless API boundary.

## Verification evidence

| Check | Result | Evidence |
| --- | --- | --- |
| Worktree and base handshake | PASS | `git status --short --branch`, `git rev-parse --show-toplevel`, `git branch --show-current`, and `git rev-parse HEAD` matched the declared worktree, branch, and base before edits. |
| Old browser product | PASS | `apps/nexus-console/README.md`, `app.js`, `index.html`, and `styles.css` are deleted in the diff; no replacement static entry was added. |
| Rust formatting | PASS | `<USER_HOME>\.cargo\bin\cargo.exe fmt --all -- --check`; nested Tauri `cargo fmt --manifest-path apps/nexus-launcher/src-tauri/Cargo.toml -- --check`. |
| Rust workspace tests | PASS | `cargo test --workspace --locked` completed with all workspace tests passing. |
| Headless Launcher targeted checks | PASS | `cargo check -p nexus-launcher --locked` and `cargo test -p nexus-launcher --locked`; 8 tests passed, including API default mode and Harness URL validation. |
| Tauri targeted checks | PASS | Nested `cargo check --locked` and `cargo test --locked`; 3 proxy/security tests passed. |
| Frontend typecheck/build | PASS | With the bundled Node 24.19.0 on PATH, `pnpm typecheck` and `pnpm build` both exited 0. Vite emitted the production assets under ignored `dist/`. |
| Tauri info | PASS | `pnpm exec tauri info` exited 0 and detected Tauri 2, WebView2, MSVC, Node, and pnpm. Ambient PATH did not expose Rust, so the explicit bundled Cargo path was used for builds. |
| Tauri native build | PASS for NSIS | `pnpm exec tauri build --bundles nsis` exited 0 and produced `apps/nexus-launcher/src-tauri/target/release/bundle/nsis/Nexus Launcher_0.1.0_x64-setup.exe`. |
| Full Tauri bundle | BLOCKED at MSI | `pnpm exec tauri build` compiled `nexus-launcher-app.exe` and produced the NSIS installer, then exited 1 while the MSI bundler hit Windows `os error 32` because another process held a file. No MSI success is claimed. |
| Tauri dev | ATTEMPTED | `pnpm exec tauri dev --no-watch` reached a ready Vite server and started the Cargo dev build. The bounded PTY ended while Cargo was compiling, so no native runtime acceptance or exit code is claimed. |
| Diff hygiene | PASS | `git diff --check` passed before staging; generated `node_modules`, `dist`, `src-tauri/target`, and `src-tauri/gen` are ignored and not staged. |
| API smoke | PASS | Built headless binaries answered `GET http://127.0.0.1:39191/launcher/status` with JSON showing `running: true`, `desired_agent_running: true`, Agent `http://127.0.0.1:39190`, and the expected temporary data root. The test listeners were stopped afterward. |
| CORS/static inspection | PASS | `App.tsx` contains no `fetch(` or `tauri://`; all UI calls use `invoke("proxy_request")`. Launcher source contains no `ServeDir`, static fallback, or startup browser call. CSP permits IPC and loopback frames only. |

## Harness replacement-process diagnosis

Repro: configure Harness on its normal loopback readiness port, start it under
the Agent supervisor, terminate only the supervised child so it exits with
code 1, and leave a replacement process serving the same readiness port. The
current Agent supervisor can classify the child as failed from
`try_wait()` before reconciling the replacement's healthy HTTP readiness. A
fresh GUI refresh now requests `/v1/harness` and `/launcher/harness` together,
replaces the prior runtime snapshot on every refresh, and shows endpoint
errors rather than retaining an old failure. Reconciliation of a healthy
replacement and refresh-log-token recency belongs to the follow-up
`crates/nexus-agent/src/supervisor.rs` phase and was intentionally not changed
here.

## Installer and security boundaries

The installer currently contains the GUI and web assets, not a self-contained
signed `nexus-launcher` or `nexus-agent` sidecar. This is documented in the
native README. Until the release pipeline ships both binaries atomically, an
installed build must use a matching helper beside the GUI or an explicit
PowerShell setting such as:

```text
$env:NEXUS_LAUNCHER_BIN = Join-Path $env:ProgramFiles 'Nexus Launcher/nexus-launcher.exe'
$env:NEXUS_CONSOLE_PORT = '3091'
```

Sidecar packaging should be revisited when signed helper artifacts and an
atomic GUI/helper version-pair upgrade test exist. The embedded Harness page
is deliberately sandboxed, sends no referrer, and accepts only validated
loopback HTTP URLs; the system-browser fallback uses the same Rust-side URL
validation.

## Scope disposition

Only the declared app, deleted Console, headless Launcher source,
architecture document, and this report are intended for the phase commit.
The worktree remains isolated and must be preserved for the parent phase to
review and merge.
