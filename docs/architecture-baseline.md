# Nexus architecture baseline

Status: Phase 11 launcher configuration and Harness access

## Purpose

Nexus is a headless control plane around the standalone DeepSeek Harness. The
implementation provides a Rust Agent, a versioned local protocol, a CLI, and
a launcher host that can also serve the replaceable Console UI. It does not
modify or vendor Harness source code.

## Non-negotiable boundaries

### Harness is an immutable upstream runtime

Harness is treated as a stable upstream black box. Nexus must not maintain a
Harness fork or edit any Harness source, package manifest, lockfile, or build
configuration. Harness updates are installed as separate, immutable release
slots and are driven through its documented CLI/runtime entry points.

Nexus may provide external profile configuration, external patches, and
separately owned plugins. If an external overlay fails, Nexus disables or
rolls it back; it does not repair the Harness source tree.

### Agent owns lifecycle and recovery

The Nexus Agent is the product's headless control plane. It owns the future
state machines for Harness process supervision, profile selection, release
promotion/rollback, checkpoints, safe mode, diagnostics, and external plugin
activation. These responsibilities must remain usable without any GUI.

The Agent state now reports its own lifecycle and the externally supervised
Harness process. Harness remains an immutable, replaceable upstream binary;
Nexus starts it only when a caller requests it and never auto-starts it during
Agent boot.

### Launcher is the host; Console/WebShell is replaceable

`nexus-launcher` is the user-facing host/runtime boundary. Its Console mode
starts or reconnects to Agent, starts a configured Harness, serves the local
WebShell, supervises Agent availability, and is the future home for tray,
notifications, and single-instance behavior. It must remain usable without a
GUI through its command line and loopback endpoints. Launcher settings are
loaded from a small `launcher.json` at the Nexus data root, so the host can be
reconfigured without taking ownership of Agent's Harness/update `config.json`.

`nexus-console` is only a view/client layer. A future Tauri or Electron shell
should load this UI (or a replacement UI) and call the launcher's local API;
it must not own lifecycle state, write Nexus files directly, or become a
Harness dependency. A browser, script, or another native toolkit can replace
that shell without changing Agent business behavior.

A dedicated native shell for the Harness Web view, if ever useful, is a
separate optional component. It is not the launcher, it does not replace the
Agent, and its failure must not remove the headless control path.

## Components

```text
nexus-protocol  versioned JSON wire types (v1)
       ^
nexus-core      paths, configuration, and state model
       ^
nexus-agent     foreground loopback HTTP server, HarnessSupervisor, updater, diagnostics, config
       ^
nexusctl        CLI client (`status`, `harness`, `profile`, `checkpoint`, `release`, `update`, `config`)
nexus-launcher  host/runtime (`console`) plus process fallback commands
apps/nexus-console  dependency-free replaceable WebShell view/client
optional native shell  Tauri/Electron container for launcher Console API
```

### Process model

These are separate processes with different responsibilities:

- `nexus-launcher.exe` is the long-lived host/console process. It starts or
  reconnects to Agent, serves the view, and supervises availability.
- `nexus-agent.exe` is the independent long-lived control-plane process. It
  listens on port 3090 and owns Nexus state and Harness supervision; Launcher
  never embeds its event loop or business state.
- `nexusctl.exe` is a short-lived command client. It sends HTTP requests to
  Agent and exits; it is not a daemon and does not host Agent.
- The configured Harness runtime is another process started and supervised by
  Agent. It remains an immutable upstream runtime.

Stopping the Console host therefore does not implicitly stop Agent. An
explicit Launcher/UI stop request is required when the independent Agent
process should end.

The Agent exposes:

- `GET /v1/health` — process health;
- `GET /v1/state` — current Agent/Harness state;
- `POST /v1/lifecycle` with `{"action":"shutdown"}` — graceful shutdown;
- `POST /v1/shutdown` — convenience graceful-shutdown endpoint.
- `GET /v1/harness` — current external Harness process information;
- `POST /v1/harness` with `{"action":"start|stop|restart"}` — explicit
  process control. `status` is also accepted as a harmless query action.
- `GET|POST /v1/profiles` — list/status the Nexus catalog or select a profile;
  selecting while Harness is starting/running returns a readable conflict and
  never performs an implicit restart.
- `GET|POST /v1/checkpoints` — list, create, or restore Nexus-only manifests.
  Restore is rejected while Harness is running; after it succeeds, Agent state
  adopts the saved profile/release metadata but does not start Harness.
- `GET|POST /v1/releases` — list/current, register an immutable slot manifest,
  promote a registered slot, or swap current with last-known-good. Promotion
  and rollback are rejected while Harness is starting/running; registration is
  metadata-only and does not install, start, or restart Harness.
- `GET|POST /v1/updates` — inspect durable update status or run one serialized
  external update job. An update clones the configured Git ref into a temporary
  Nexus `downloads/` candidate, optionally runs explicitly configured build and
  verify commands, then atomically publishes the candidate as a release slot.
  The command never edits or vendors the upstream Harness checkout. A failed
  job is recorded in `update-state.json` and its candidate is discarded.
- `GET|POST /v1/diagnostics` — list or collect bounded Nexus-only diagnostic
  bundles. Collection copies runtime metadata and text logs into a new
  `diagnostics/<id>/` directory, redacting common credential-bearing lines and
  omitting binary payloads. It never traverses `$HOME/.dsh`, Harness data, or
  the process environment.
- `GET|POST /v1/config` — inspect or mutate the Nexus-owned Harness launch and
  external update specifications. Mutations validate all paths, arguments,
  refs, and URLs before an atomic write to `config.json`; changing Harness
  configuration while it is running, or update configuration while an update
  job is running, returns a conflict instead of interrupting either process.
  Responses redact credential-shaped argument values, while the on-disk Nexus
  configuration remains the caller-owned source of truth.

The Harness response always includes a `state` and may include `pid`,
`exit_code`, `error`, and timestamps. A missing `harness.program` is a normal
control-plane-only configuration: start returns a readable
`harness_not_configured` error instead of panicking. Stop is idempotent when no
child is attached. A stop first waits for natural process exit for a bounded
grace period (five seconds by default), then uses the platform-neutral Tokio
kill fallback.

The default listener is `127.0.0.1:3090`, deliberately separate from the
current Harness Web port. No remote bind option is exposed in this phase.
Authentication is intentionally deferred while the listener remains strictly
loopback-only; adding a local authentication token is a protocol change that
must be specified before GUI integration.

## Data and path policy

Nexus-owned files are separate from `$DSH_HOME`. The default Nexus data root
is resolved with platform APIs/environment conventions:

- Windows: `%LOCALAPPDATA%/Nexus`;
- macOS: `~/Library/Application Support/Nexus`;
- Linux/Unix: `$XDG_STATE_HOME/nexus`, or `~/.local/state/nexus`.

`NEXUS_DATA_DIR` is an explicit override for development and tests. The
runtime never hard-codes a drive letter or assumes Windows path separators.

The first path model reserves directories for `logs`, `checkpoints`,
`releases`, `downloads`, and `run`, and stores the profile catalog in
`profiles.json`. Release slot manifests live below `releases/<id>/manifest.json`
and the current/last-known-good pointers are atomically published in
`release-pointers.json`. Update execution status is published separately in
`update-state.json`; command stdout/stderr use release-specific files under
`logs/`. Nexus does not copy credentials, Harness sessions, or secret
environment values into Nexus state.

Profiles are Nexus-owned names. The default is `web`; names are limited to
ASCII letters, digits, `.`, `_`, and `-` with a bounded length. The active name
is persisted with the known names in `profiles.json`. A profile is rendered
into Harness launch arguments only when a Nexus-owned `HarnessLaunchSpec.args`
entry explicitly contains the `{profile}` placeholder. Nexus does not infer a
Harness CLI flag, edit Harness configuration, inject `DSH_HOME`, or read
`$HOME/.dsh`.

Checkpoints are JSON manifests under the Nexus-owned `checkpoints/` directory.
They record a safe ID, timestamp, profile, release metadata, optional note,
and a verifiable Nexus state summary. Writes use an atomic same-directory
publication. Restore reads and applies only this Nexus metadata; it never
copies, rewrites, or restores `.dsh` user data, Harness sessions, or
credentials.

Phase 2 reads optional Harness launch configuration from the Nexus-owned
`config.json` under the `harness` key (a direct launch-spec object is also
accepted):

```json
{
  "harness": {
    "program": "/opt/dsh-harness/bin/harness",
    "args": ["--headless"],
    "working_dir": "/opt/dsh-harness",
    "readiness_url": "http://127.0.0.1:8080/health",
    "readiness_timeout_secs": 30
  }
}
```

Harness launch fields may opt into the active immutable release slot with the
placeholders `{release}` (the safe slot ID) and `{release_root}` (the
canonical `releases/<id>` directory). `{profile}` remains available for the
explicit profile name. For example, `program: "{release_root}/bin/harness"`
and `working_dir: "{release_root}"` make the selected release the executable
without changing the upstream tree. If a release placeholder is configured
while no release is current, start fails with a configuration error instead of
falling back to an arbitrary directory. Static program paths continue to work
unchanged.

The fields can be overridden explicitly for development and tests with
`NEXUS_HARNESS_PROGRAM`, `NEXUS_HARNESS_ARGS` (JSON array or whitespace
separated), `NEXUS_HARNESS_WORKING_DIR`, `NEXUS_HARNESS_READINESS_URL`, and
`NEXUS_HARNESS_READINESS_TIMEOUT_SECS`. `NEXUS_DATA_DIR` selects the Nexus
root containing `config.json`, `state.json`, `profiles.json`, `checkpoints/`,
`releases/`, `downloads/`, `update-state.json`, `diagnostics/`, and `logs`; it does not select or
copy `$HOME/.dsh`, `DSH_HOME`, Harness credentials, or Harness session data.
Nexus never defaults to `$HOME/.dsh`.

The Launcher reads an optional `<data-root>/launcher.json` independently of
the Agent-owned `config.json`:

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

The effective precedence is `CLI > launcher.json > environment > built-in
defaults`. `--data-dir` selects the root before that file is loaded and is
therefore intentionally not a field in `launcher.json`. CLI overrides are
`--agent`, `--port`, `--console-dir`, `--console-port`, `--wait-secs`, and
`--open`/`--no-open`; environment overrides are
`NEXUS_DATA_DIR`, `NEXUS_AGENT_PORT`, `NEXUS_AGENT_BIN`, `NEXUS_CONSOLE_DIR`,
`NEXUS_CONSOLE_PORT`, `NEXUS_LAUNCHER_WAIT_SECS`, and `NEXUS_CONSOLE_OPEN`.
Invalid file values fail closed with the config path in the error; invalid
environment values fall back to defaults. Unknown JSON fields are ignored for
forward compatibility.

`state.json` is Nexus runtime metadata, published through a temporary file and
an atomic replace in the same Nexus root. It is kept separate from the
external Harness working/data directory. Supervisor stdout and stderr are
appended to `logs/harness.stdout.log` and `logs/harness.stderr.log`.

Diagnostics bundles are stored under `diagnostics/<id>/`. Each bundle contains
only a bounded allowlist of Nexus metadata and copied text logs. Sensitive
credential-shaped lines are replaced with `[REDACTED]`, binary payloads are
omitted, and oversized files are truncated. The API returns the local bundle
directory and file metadata so a GUI or script can attach the bundle without
receiving raw secrets over the wire.

Readiness probes are deliberately limited to loopback targets (`localhost`,
`127.0.0.1`, or `[::1]`) and plain HTTP, so the optional URL cannot turn the
Agent into a remote-network/SSRF probe. HTTPS or a non-loopback URL is rejected
with a clear error until a separately specified, authenticated design exists.

The optional update plan is configured under the `update` key in the same
`config.json` (or through the `NEXUS_UPDATE_*` environment overrides):

```json
{
  "update": {
    "source": "https://github.com/example/dsh-harness.git",
    "ref_name": "main",
    "git_program": "git",
    "build_program": "cargo",
    "build_args": ["build", "--release"],
    "verify_program": "cargo",
    "verify_args": ["test", "--locked"],
    "timeout_secs": 900
  }
}
```

Build and verify arguments may use `{source}`, `{release}`, and `{ref}`
placeholders. Programs are spawned directly with argument vectors; no shell
interpolation is performed. Source URLs with embedded credentials, unsafe refs,
control characters, and unbounded argument lists are rejected. A job is
serialized per Agent and stale `running` state is converted to a durable
failure after an Agent restart because Nexus cannot attach to an old child.

The configuration API uses the same v1 protocol as the CLI. `nexusctl config
status` shows a redacted summary, while `nexusctl config clear-harness` and
`nexusctl config clear-update` remove one owned section after the corresponding
runtime has stopped/returned idle. Setting sections is currently an API-level
operation so a future Tauri, Electron, browser, or script frontend can supply
typed forms without taking ownership of persistence or process coordination.

`nexus-launcher` is the host/runtime boundary around the Agent. `console` (also
the no-argument/double-click mode) resolves the sibling `nexus-agent` (or an
explicit `--agent`/`NEXUS_AGENT_BIN`), creates a recoverable lock and launch
record below `run/`, redirects Agent logs into the Nexus `logs/` directory,
waits for loopback health, serves `apps/nexus-console/` on
`127.0.0.1:3091`, and exposes local `/launcher/*` controls for the UI. It
starts a configured Harness after Agent is healthy and watches Agent so a
crash can be recovered without another manual command. Agent is always a
separate operating-system process: stopping the Console host does not
implicitly stop Agent. Use the UI's explicit stop action or
`nexus-launcher stop` when Agent should end.

The Launcher also exposes `GET /launcher/harness` and
`POST /launcher/harness` with `{"action":"open"}`. These endpoints inspect
only a bounded tail of the Nexus-owned `logs/harness.stdout.log` and
`harness.stderr.log`, select the newest token-bearing loopback HTTP URL, and
allow the user to open it in the system browser. The response includes the
latest extracted token for local display/copy in Console. URLs with remote
hosts, HTTPS, credentials, or control characters are ignored; no arbitrary
path or URL is opened, and no `.dsh`/Harness source is read.

`start`, `run`, `stop`, `status`, and `logs` remain script/recovery fallbacks.
They keep the existing guarantees: `start` returns only after health, `run`
keeps Agent attached in the foreground, and `stop` calls the Agent shutdown
endpoint and waits for the listener to disappear rather than killing an
arbitrary PID. A lock older than the bounded stale interval is recoverable only
after a fresh health probe confirms that no Agent is serving the configured
port.

The Console view lives under `apps/nexus-console/`. It validates that its API
base is loopback-only, renders Agent/Harness/profile/release/update/checkpoint/
diagnostic/config status, and sends explicit v1 actions. When served by
`nexus-launcher console`, it also uses `/launcher/status` and
`/launcher/agent` for Agent lifecycle controls, plus `/launcher/harness` to
open the Harness UI and display/copy its latest token. If those routes are
absent, the page deliberately degrades to a read/action client for a
separately started Agent; this is the development-only Python static preview
path. The view stores no runtime state and has no dependency on Tauri or
Electron. A native shell can load the same assets or replace them entirely.

## Current scope and exclusions

This phase does not add native Tauri/Electron packaging, Harness source
dependencies, plugin marketplaces, recommendations, advertising, cloud sync,
remote control, or Launcher API authentication. The Harness token viewer only
surfaces a token that the opaque upstream process already printed to a local
loopback URL; it is not a new Nexus authentication protocol. The single-entry
Rust host is available via
`nexus-launcher console`; a dependency-free static WebShell view is included
under `apps/nexus-console/`, while native packaging remains a separate
follow-up slice against the launcher/Agent protocols. Browser access is limited
to the fixed local CORS origins documented above. Release registration/promotion remain
explicit metadata operations;
the update executor installs a verified immutable slot but does not silently
change the active pointer or start Harness. Once a slot is explicitly promoted,
the supervisor resolves `{release_root}` from the catalog and performs launch
from that canonical directory; it never guesses a release from the process
working directory. Diagnostics are bounded and redacted as described above;
they are not a general filesystem archive. Profile selection does not claim to
understand Harness internals, and checkpoint restore remains metadata-only.
