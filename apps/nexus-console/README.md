# Nexus Console / WebShell

This directory is a dependency-free browser client for the loopback Nexus
Agent. It is intentionally not a second business layer: all state changes go
through the versioned `/v1/*` API, and all sensitive configuration values are
redacted by the Agent before they reach the page.

## Single-entry host (recommended)

Build the workspace first, then start the launcher Console host. It serves this
directory, starts or reconnects to the Agent, starts a configured Harness, and
keeps watching the Agent. No separate Python process or `nexusctl` command is
needed for normal use:

```powershell
& "E:\git\dsh-nexus\target\release\nexus-launcher.exe" console `
  --data-dir "D:\dsh-local\nexus-data"
```

The explicit `console` word is optional: launching `nexus-launcher.exe` with no
command enters the same host mode, which is the future double-click path. Use
`--agent PATH` when the Agent binary is not beside the launcher, and
`--console-dir PATH` when the UI is packaged separately. The host listens on
`http://127.0.0.1:3091/`; its local `/launcher/*` endpoints provide Agent
lifecycle controls for the view. Agent remains an independent process when the
Console host exits; use the explicit stop button or `nexus-launcher stop` to
end it.

`nexusctl` is still available as a short-lived recovery/automation client. It
does not host Agent or replace the launcher.

## Development-only preview

The Python server is only a static asset preview. It cannot start, stop, or
supervise Agent/Harness, so it is not the product entry point. Start the Agent
separately only when testing the view in isolation:

```text
python -m http.server 3091 --directory apps/nexus-console
```

Open <http://127.0.0.1:3091/> and leave the Agent API field at
`http://127.0.0.1:3090` (or enter another loopback port). The page labels itself
“静态预览” when launcher routes are absent and leaves Agent lifecycle buttons
disabled by behavior (they explain how to start the real host). A future Tauri
or Electron shell should load the same view or replace it while keeping
`nexus-launcher`/Agent as the source of truth.

The Agent allows browser requests from the fixed local Console origins
`http://127.0.0.1:3091`, `http://localhost:3091`, and
`http://[::1]:3091`. Keep the static server on port `3091`; a different port
is intentionally rejected by the loopback CORS policy. The Agent remains bound
to loopback and never enables wildcard or credentialed remote access.
