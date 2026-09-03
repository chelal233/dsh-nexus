# Nexus architecture baseline

Status: Phase 7 diagnostics collection (headless MVP)

## Purpose

Nexus is a headless control plane around the standalone DeepSeek Harness. The
first implementation slice provides a Rust Agent, a versioned local protocol,
and a CLI. It does not modify or vendor Harness source code.

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

### Console/WebShell is replaceable

Tauri is not a business layer. A future Tauri Console/WebShell may provide
windows, tray behavior, notifications, and a Harness Web view, but it must
consume the Agent protocol rather than own lifecycle state or write runtime
files directly. Electron, a browser, or a script can replace it without
changing Agent behavior.

## Components

```text
nexus-protocol  versioned JSON wire types (v1)
       ^
nexus-core      paths, configuration, and state model
       ^
nexus-agent     foreground loopback HTTP server, HarnessSupervisor, updater, diagnostics
       ^
nexusctl        CLI client (`status`, `harness`, `profile`, `checkpoint`, `release`, `update`)
```

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

## Current scope and exclusions

This headless MVP does not add Tauri, Electron, Harness source dependencies,
plugin marketplaces, recommendations, advertising, cloud sync, remote control,
or authentication. The replaceable UI can be added later against the v1 Agent
protocol. Release registration/promotion remain explicit metadata operations;
the update executor installs a verified immutable slot but does not silently
change the active pointer or start Harness. Once a slot is explicitly promoted,
the supervisor resolves `{release_root}` from the catalog and performs launch
from that canonical directory; it never guesses a release from the process
working directory. Diagnostics are bounded and redacted as described above;
they are not a general filesystem archive. Profile selection does not claim to
understand Harness internals, and checkpoint restore remains metadata-only.
