# Nexus Launcher

Native Tauri 2 operator shell for the independent Nexus Agent and immutable
external Harness runtime.

## Runtime boundary

The visible UI is a React and TypeScript application. It never calls the
Agent with browser `fetch`, so the UI does not depend on CORS or on a custom
`tauri://` origin. Tauri Rust commands validate a fixed loopback route
allowlist and proxy `GET` and `POST` requests to the headless Launcher API.
The only state owned here is window and tray state. Agent business state,
Harness supervision, profiles, checkpoints, releases, updates, and
diagnostics remain in the separate Rust Agent.

On startup the native side probes and, when possible, starts
`nexus-launcher api --no-open`. Helper resolution is ordered as follows:

1. `NEXUS_LAUNCHER_BIN` when it names an existing file;
2. the helper staged in the Tauri resource directory;
3. a `nexus-launcher` executable beside the native application;
4. `target/debug` and `target/release` candidates found near a Cargo target
   layout during local development.

If no helper is available, the UI keeps the error visible and reports the
exact configuration action. The native shell only controls the helper process
it starts itself. A pre-existing or separately started listener on the same
loopback port is reported as unavailable rather than reused.

For every start, the native shell creates a fresh private capability and passes
it only through the owned helper's environment. The helper removes it before
spawning Agent or Harness processes, omits it from status and CLI arguments,
and requires it on every route except bootstrap status/handshake. Before an
operation, native code verifies an HMAC challenge bound to the pinned
data-root/instance and sends the capability and JSON body only afterward on the
same TCP connection. An unrelated listener that wins a port-rebind race can
observe the public challenge, but cannot receive or execute the operation.

## Development

Install the frontend dependencies and run a native development window:

```text
pnpm install
pnpm tauri dev
```

The local Tauri CLI is provided by `@tauri-apps/cli`; a globally installed
`cargo-tauri` command is not required. `pnpm exec tauri` is the equivalent
explicit invocation when the CLI is not on PATH.

Useful checks:

```text
pnpm typecheck
pnpm build
pnpm tauri build
```

The frontend dev server is only a development asset server. Production
packaging embeds the built assets through Tauri. `pnpm tauri build` first
builds the Rust helper and stages it into the Tauri resources directory, so
the NSIS/MSI output contains the matching headless helper. The helper remains
a separate process and the app does not vendor or modify Harness source.

## Helper override

An explicit helper path is still available for development, signed replacement
builds, or rollback testing. Before launching the native app, set it in
PowerShell:

```text
$env:NEXUS_LAUNCHER_BIN = 'C:\Program Files\Nexus Launcher\nexus-launcher.exe'
$env:NEXUS_CONSOLE_PORT = '3091'
```

## Native behavior

- Closing the main window hides it to the system tray. The tray menu can show
  the window or quit the native shell.
- The single-instance plugin focuses the existing window when a second launch
  is attempted.
- Desktop notifications are used for close-to-tray feedback.
- The official global-shortcut plugin registers Ctrl+Shift+N to restore the
  window. If a platform cannot register that shortcut, the native window and
  tray menu remain usable.
- Settings offers System, Light, and Dark themes. The selected mode is stored
  in local storage and System follows operating-system preference changes.

The Harness page only embeds a validated loopback HTTP URL returned by
`GET /launcher/harness`. Its token is masked by default and is read from a
bounded Nexus-owned log tail. The system-browser button calls
`POST /launcher/harness` with `{"action":"open"}`, which keeps URL validation
and external opening in the Rust Launcher API.
An Agent or Harness stop/restart clears the token and unmounts the iframe before
the request is sent. A failed action stays credential-closed until a positively
stopped runtime or a new matching run session is observed. PID-less Harness
observations never expose credentials because their process continuity cannot
be proven.
