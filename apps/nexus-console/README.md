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
`http://127.0.0.1:3091/`; its local `/launcher/*` endpoints provide Agent and
Harness controls for the view. The “打开 Harness” action opens the newest
loopback authentication URL observed in the bounded Harness stdout/stderr
tail, while the page exposes the extracted token for copying. No Harness
source or `$HOME/.dsh` data is read. Agent remains an independent process when
the Console host exits; use the explicit stop button or `nexus-launcher stop`
to end it.

## Launcher configuration

The launcher reads an optional `<data-root>/launcher.json`, separate from the
Agent-owned `config.json`:

```json
{
  "schema_version": 1,
  "agent_program": "E:/git/dsh-nexus/target/release/nexus-agent.exe",
  "agent_port": 3090,
  "console_dir": "E:/git/dsh-nexus/apps/nexus-console",
  "console_port": 3091,
  "wait_secs": 20,
  "open_browser": true
}
```

The effective precedence is CLI > `launcher.json` > environment > built-in
defaults. `--data-dir` chooses the root before `launcher.json` is loaded, so
it is intentionally not a field in that file. CLI overrides include
`--agent`, `--port`, `--console-dir`, `--console-port`, `--wait-secs`, and
`--open`/`--no-open`. Environment overrides are
`NEXUS_DATA_DIR`, `NEXUS_AGENT_PORT`, `NEXUS_AGENT_BIN`, `NEXUS_CONSOLE_DIR`,
`NEXUS_CONSOLE_PORT`, `NEXUS_LAUNCHER_WAIT_SECS`, and `NEXUS_CONSOLE_OPEN`.

The local Launcher API adds `GET /launcher/harness` for the latest safe URL
and token, and `POST /launcher/harness` with
`{"action":"open"}` to invoke the system browser. It only accepts plain HTTP
loopback hosts (`127.0.0.1`, `localhost`, or `::1`); missing/remote URLs are
not opened.

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

The Agent allows browser requests from loopback Console origins on the
configured Console port (`3091` by default): `127.0.0.1`, `localhost`, and
`[::1]`. `nexus-launcher` passes `NEXUS_CONSOLE_PORT` to a newly spawned Agent
so a `launcher.json`/CLI Console port works end to end. A directly started
Agent uses the default unless its environment sets the same variable. The
Agent remains bound to loopback and never enables wildcard or credentialed
remote access.
