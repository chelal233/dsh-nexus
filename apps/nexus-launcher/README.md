# Nexus Launcher

Native Tauri 2 operator shell for the independent Nexus Agent and immutable
external Harness runtime.

## Runtime boundary

The visible UI is a React and TypeScript application. It never calls the
Agent with browser `fetch`; Tauri Rust commands validate a fixed loopback
allowlist and proxy bounded `GET` and `POST` JSON requests directly to the
Agent's versioned `/v1/*` API. Agent business state, Harness supervision,
profiles, checkpoints, releases, updates, and diagnostics remain in the
separate `nexus-agent` process.

The native side uses `nexus-launcher-core` for Agent HTTP requests and for
resolving, starting, probing, and stopping the independent Agent. The app does
not spawn, package, or require `nexus-launcher.exe`. The executable remains a
legacy compatibility client for existing headless scripts and is not a GUI
runtime dependency.

Agent resolution is ordered as follows:

1. an explicit path passed to the core runtime;
2. `NEXUS_AGENT_BIN` when it names an existing file;
3. an `nexus-agent` executable beside the native application;
4. the Tauri resource root or its `resources/nexus-agent` subdirectory
   (including the platform `.exe` name);
5. nearby `target/debug` and `target/release` candidates during local
   development.

The Agent API binds to loopback and carries the data-root and instance
identity headers when a client has a positively probed identity. Requests and
responses are bounded, and only the documented `/v1/*` route allowlist is
forwarded. A separately started Agent can be probed and used when its
identity matches the configured data root; the native shell never attaches to
an unrelated listener.

The Agent HTTP/JSON API is the cross-language boundary. Future Electron or
another UI consumes the same API rather than linking the Rust crate directly;
`nexus-launcher-core` is a current Rust bridge and compatibility convenience.

## Development

Install the frontend dependencies and run a native development window:

```text
pnpm install
pnpm tauri dev
```

Useful checks:

```text
pnpm typecheck
pnpm test
pnpm build
pnpm tauri build
```

`pnpm tauri build` first runs `pnpm prepare:agent`, which builds the locked,
local `nexus-agent`, `nexus-launcher`, and `nexus-cli` packages and stages their
three matching executables as Tauri resources before the installer is
assembled. The bundle resource map gives each `resources/nexus-*` binary an
empty target, flattening all three into the bundle resource root (the same
install directory as the launcher executable on the supported Windows bundle
layout); the resolver also accepts the platform's `<exe>/resources` layout as
a bounded fallback. The staging step touches only the exact generated
`src-tauri/resources/nexus-agent*`, `nexus-launcher*`, and `nexusctl*` files; it
does not download, start, or modify Harness source or data. The installer
therefore runs without a workspace `target` directory or `NEXUS_AGENT_BIN`,
and the GUI never requires any of these compatibility binaries at runtime.

## Agent override

An explicit Agent path is available for development, signed replacement
builds, or rollback testing. Before launching the native app, set it in
PowerShell:

```text
$env:NEXUS_AGENT_BIN = 'C:\Program Files\Nexus Agent\nexus-agent.exe'
$env:NEXUS_AGENT_PORT = '3090'
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

The Harness view consumes `GET /v1/harness/ui`, an Agent-owned bounded JSON
contract for the current validated loopback URL/token session. The Agent
publishes credentials for a running PID-owned Harness generation, or for a
PID-less recovered descendant only after its fresh durable log-session boundary
matches. PID-less recovery is read-only: lifecycle controls remain disabled
because the Agent cannot safely claim the replacement process. The UI fails
closed when that contract is unavailable. Stop/restart actions clear displayed
credentials before the request is sent.
