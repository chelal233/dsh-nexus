# Nexus architecture baseline

Status: Phase 13 native Tauri launcher and headless API boundary

## Purpose

Nexus is a headless control plane around the standalone DeepSeek Harness. The
implementation provides a Rust Agent, a versioned local protocol, a CLI, a
headless Launcher API, and a native Tauri operator shell. It does not modify
or vendor Harness source code.

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

### Launcher is the host; native GUI is replaceable

`nexus-launcher` is the user-facing host/runtime boundary. Its `api` mode
starts or reconnects to Agent, starts a configured Harness, binds only a
loopback control API, supervises Agent availability, and never serves HTML or
opens a browser on startup. The historical `console` command remains a
compatibility alias for `api`. Launcher settings are loaded from a small
`launcher.json` at the Nexus data root, so the host can be reconfigured
without taking ownership of Agent's Harness/update `config.json`.

`apps/nexus-launcher` is the native Tauri 2 shell. It owns navigation, window
state, tray behavior, theme preference, and safe presentation. Its Rust side
proxies a fixed allowlist of Launcher and Agent loopback routes; the React
frontend never relies on browser CORS or a custom-origin HTTP request. The
shell can be replaced by another native toolkit or a script without changing
Agent business behavior.

The embedded Harness Web view is optional and remains an opaque upstream
surface. Its URL is accepted only when the Launcher has validated it as plain
HTTP on loopback. A system-browser fallback uses the same validated Launcher
route.

## Components

```text
nexus-protocol  versioned JSON wire types (v1)
       ^
nexus-core      paths, configuration, and state model
       ^
nexus-agent     foreground loopback HTTP server, HarnessSupervisor, updater, diagnostics, config
       ^
nexusctl        CLI client (`status`, `harness`, `profile`, `checkpoint`, `release`, `update`, `config`)
nexus-launcher  headless Launcher API (`api`, `console` alias) plus process fallback commands
apps/nexus-launcher  native Tauri 2 shell with Rust loopback proxy
```

### Process model

These are separate processes with different responsibilities:

- `nexus-launcher.exe` is the long-lived headless API host. It starts or
  reconnects to Agent, exposes Launcher controls, and supervises availability.
- `nexus-launcher-app.exe` is the replaceable native Tauri shell. It starts or
  probes the headless API helper, proxies local requests through Rust, and
  owns only GUI, tray, notification, theme, and single-instance state.
- `nexus-agent.exe` is the independent long-lived control-plane process. It
  listens on port 3090 and owns Nexus state and Harness supervision; Launcher
  never embeds its event loop or business state.
- `nexusctl.exe` is a short-lived command client. It sends HTTP requests to
  Agent and exits; it is not a daemon and does not host Agent.
- The configured Harness runtime is another process started and supervised by
  Agent. It remains an immutable upstream runtime.

Stopping the native shell or headless Launcher host does not implicitly stop
Agent. An explicit Launcher/UI stop request is required when the independent
Agent process should end.

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
  job is recorded in `update-state.json` and its candidate is discarded. Once
  an install is accepted, a detached Agent-owned task retains the executor gate,
  candidate, and every spawned command through process wait/reap and durable
  terminal-state publication. Cancelling the HTTP request therefore cannot
  orphan the command or authorize a concurrent install/configuration change.
  Spawned commands are also kill-on-runtime-drop; after an Agent restart, a
  durable stale `running` record is failed closed before another install begins.
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
`exit_code`, `error`, and timestamps. Agent responses also publish the current
supervisor `generation` plus the complete Nexus log-session boundary: run ID,
generation, per-run log filenames, byte watermarks, open-file identities, and
the active ownership reservation. The Launcher compares
that observation with the durable local marker before exposing an
authentication URL, so a runtime observation cannot be combined with a
different Harness run's log boundary. A missing `harness.program` is a normal
control-plane-only configuration: start returns a readable
`harness_not_configured` error instead of panicking. Stop is idempotent when no
child is attached. A stop first waits for natural process exit for a bounded
grace period (five seconds by default), then uses the platform-neutral Tokio
kill fallback.

Harness supervision uses the existing `starting` state as a bounded recovery
state; no new protocol enum is introduced. A configured loopback readiness URL
is probed while the direct child is starting. If that child unexpectedly exits
with any code, Nexus publishes `starting` with the external `pid`, `exit_code`, and
`error` cleared, then re-probes the same loopback URL until the configured
deadline. A healthy replacement or descendant is reported as `running` with no
PID when Nexus has no attachable child handle; stale recovery metadata is
cleared. If the deadline expires, the actual exit code and a readiness timeout
are reported as `failed`. Without a readiness URL, a non-zero child exit remains
an immediate `failed` result. Start returns the persisted `starting` snapshot;
an ownership gate prevents status and monitor polling from changing the child
state before that snapshot is durable. Only after persistence does an independent
generation-bound task begin child monitoring and readiness, so cancellation of
the API request cannot strand that state or let a short bootstrap mask a metadata
failure. A cancellation before initial persistence, or an initial persistence
failure, assigns cleanup to one detached owner which acknowledges kill/wait/reap
completion. If the gated bootstrap already exited and has configured
readiness, that owner retains bounded unattached replacement recovery rather than
allowing a duplicate bootstrap. Stop similarly transfers the child to a
detached stop owner before awaiting its result; request cancellation cannot
drop the only process handle, wait/kill errors restore it for retry, and a
`stop_pending` guard rejects starts during that transfer. Restoring a child also
re-arms the ownership handoff gate, then resumes its monitor and any pending
readiness owner; a stop error therefore cannot leave a healthy child permanently
in `starting`. Start, stop, and
restart share the supervisor's serialized lifecycle guard. Agent control-plane
transitions that require Harness to remain stopped (profile selection,
checkpoint create/restore, release promotion/rollback, and Harness launch
configuration writes) acquire that exact same guard across their stopped check
and metadata publication. This is a positive quiescence check: the state must
be terminal and there must be no owned child, PID, launch reservation, start,
stop, readiness, recovery, or unattached-monitor owner. A `failed` label alone
does not authorize a selection change. The guard is never held across the readiness
deadline, and status polling stays independent. A second start also polls an exited bootstrap through the same
recovery transition as the monitor. When replacement readiness is configured,
that transition schedules recovery and rejects the second bootstrap instead of
skipping directly to a new process. Every asynchronous Harness publication is
generation-bound. Full
Agent snapshots additionally carry a monotonically increasing in-process Agent
revision, preventing a stale Agent snapshot from overwriting a newer one.
Whenever polling creates a recovery task, Nexus schedules that detached owner
before awaiting persistence. Request cancellation can therefore drop only the
observer, never the sole owner of a recovery already marked as started.

Before process creation, Agent durably publishes a session reservation and a
`starting` snapshot with no PID. A crash before or after spawn therefore leaves
an explicit recovery intent instead of an older terminal snapshot that could
authorize a duplicate process. On Agent startup, `recover_unattached` may probe the configured loopback URL for
persisted `starting`, `running`, or `failed` observations. A healthy endpoint
restores `running` without reviving or claiming the persisted PID. If the probe
is not healthy and the durable session still owns an active run, Agent remains
in bounded recovery and refuses another spawn. A recovered `running` process
with no child handle has a continuous bounded readiness owner. Loss of that
signal immediately returns to `starting`, advances the run ID and EOF token
watermarks, and requires a token emitted after that boundary when readiness
returns. Persisted `stopped` and `detached` observations without an active
reservation are never probed or resurrected.
A `running` process with an attached child handle also retains a serialized
readiness owner when a readiness URL is configured. The first failed probe
durably advances the generation/run ID and log EOF watermarks, publishes
`starting`, and keeps the same child PID under Agent ownership. The process
monitor transfers to the new generation, so a second spawn remains forbidden.
Only readiness observed before the bounded recovery deadline can republish
`running`; token scanning therefore accepts only output written after the gap.
The durable `launch_pending` reservation remains set for as long as the Agent
owns the attached child, including recovered `running` and timeout `failed`
states. An Agent restart therefore continues to reject a duplicate spawn even
though the new Agent cannot inherit the old process handle. Only explicit
stop/reap or a confirmed child-exit path clears that reservation. Timeout
publishes `failed` while retaining the child/PID owner: start remains rejected
and an explicit stop can still terminate and reap that process.
A pending replacement recovery and an externally recovered `running`
observation have no attached child handle. Nexus rejects stop with
`409 harness_unattached` and continues to reject start, rather than claiming the
external process stopped or launching a duplicate.
Because an unattached process has no durable process identity, Launcher never
publishes its URL or token. An attached process is required for credential
presentation, and the first PID-less readiness gap advances the run boundary;
availability is sacrificed rather than reusing a credential across a restart
that Nexus cannot prove did not occur.

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
and a selection-only state containing the same profile and release. New
manifests never persist Agent lifecycle, Harness runtime, PID, token, or log
state. Legacy manifests containing the former runtime summary remain readable;
those extra fields are ignored and are never applied by restore. Writes use an
atomic same-directory publication. Restore validates that an optional release still names a
registered, canonical slot and first writes a two-phase intent under `run/`
containing the exact prior and target profile/current/LKG selections. A
detached owner holds the supervisor lifecycle gate while publishing those
stores and their derived Agent runtime metadata, then marks the intent
committed. Cancellation cannot abandon a prepared restore. Startup and every
later lifecycle transition recover a prepared intent to the exact prior
selection, while a committed intent must exactly match the target selection or
startup fails closed; only then is the intent cleared. Later Harness start and
restart operations therefore cannot observe a mixed selection and resolve the
restored release; a checkpoint with no release clears the current pointer.
If publishing the committed phase reports an ambiguous post-rename durability
error, Nexus re-reads the journal: an observed committed phase completes the
target, an observed prepared phase rolls back the exact prior selection, and an
unreadable or changed journal remains for fail-closed recovery instead of
guessing and rolling back a possibly committed target.
State, Harness, profile, and release reads use the same gate and return a
recovery error instead of publishing a pending mixed view.
Restore applies only this Nexus Harness-selection
metadata; it never copies, rewrites, or restores `.dsh` user data, Harness
sessions, or credentials.

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
  "console_port": 3091,
  "wait_secs": 20
}
```

The effective precedence is `CLI > launcher.json > environment > built-in
defaults`. `--data-dir` selects the root before that file is loaded and is
therefore intentionally not a field in `launcher.json`. CLI overrides are
`--agent`, `--port`, `--console-port`, and `--wait-secs`; environment overrides
are `NEXUS_DATA_DIR`, `NEXUS_AGENT_PORT`, `NEXUS_AGENT_BIN`,
`NEXUS_CONSOLE_PORT`, and `NEXUS_LAUNCHER_WAIT_SECS`. Legacy directory and
browser preference fields remain parse-compatible but are ignored because the
headless API never serves HTML or opens a browser at startup.
Invalid file values fail closed with the config path in the error. The native
Tauri shell also fails closed when an explicitly present
`NEXUS_LAUNCHER_API_PORT` or `NEXUS_CONSOLE_PORT` is invalid; the startup error
is shown in the UI instead of silently connecting to port 3091. Unknown JSON
fields are ignored for forward compatibility.

`state.json` is Nexus runtime metadata, published through a synced temporary
file and an atomic replace in the same Nexus root. Unix also syncs the
containing directory after rename; Windows uses a write-through replace. It is
kept separate from the
external Harness working/data directory. Every fresh Harness spawn uses unique
Nexus-owned files such as `logs/harness-<run>.stdout.log` and
`logs/harness-<run>.stderr.log`; the reusable legacy filenames are never an
active run source. Before spawn, Agent creates and syncs those exact handles
(and the containing log directory on Unix), then atomically publishes and
syncs `run/harness-log-session.json` with a unique run
ID, generation, safe filenames, and separate
stdout/stderr watermarks plus cross-platform file identities (Windows volume
serial/file index or Unix device/inode). Only then are the same handles
transferred to the child. On upgrade or Agent startup with no current marker,
new empty per-run files become a safe baseline: historical tokens are not
adopted. A bootstrap parent
that hands readiness to a descendant remains the same logical start/run and
therefore retains the same marker; an explicit stop plus start/restart creates a
new marker. A fresh start also re-reads the marker and refuses to spawn if a
different Agent writer replaced it. The supported Launcher path serializes
bootstrap with an OS-owned exclusive lock on persistent
`run/agent-bootstrap.lock`; it never deletes a lock based on age. The Agent
itself owns the separate `run/agent.lock` for its complete process lifetime, so
two Agents cannot serve one data root even on different ports. Agent health
advertises an OS file identity for the canonical data-root directory (Windows
volume/file ID or Unix device/inode) and a per-process instance ID. Launcher
supplies a fresh expected instance nonce at spawn, waits for an exact health
match, and correlates launch metadata with that same nonce. On Windows the
Launcher retains the exact `CreateProcessW` process handle until nonce health
and launch-record publication complete; failure cleanup terminates and waits
on that handle and never targets a reusable bare PID. `run/agent.json` is
published from a synced temporary file by atomic replacement, with Unix
directory fsync or Windows write-through semantics.
Start, stop, and restart share one serialized lifecycle guard; status polling
does not wait on that guard, and background readiness does not retain it.
Runtime metadata writes verify the Harness generation/current runtime and the
Agent revision, so a late recovery, exit, or Agent API snapshot cannot overwrite
a newer explicit state. Once a child has exited or been reaped, its PID is
cleared while the exit code remains available as historical evidence. Agent
startup also scrubs a persisted PID from unattached `stopped` and `failed`
snapshots.
Readiness ownership uses an in-memory operation epoch distinct from the durable
logical run generation. A bootstrap-parent exit converts the one existing
readiness task into the recovery owner; it does not launch a competing probe
owner. On transfer that task reads the current `RecoveryState` owner epoch and
deadline instead of retaining its original start deadline; any stale-epoch
completion exits without publishing. Stop failures restore the child under the
same logical generation, so the durable log-session join remains valid. If an Agent restart finds a
write-ahead launch reservation for a configuration with no readiness contract,
it marks that run failed and durably clears the reservation. A recovery owner
that reaches its configured deadline does the same. This is the explicit
compatibility boundary: a no-readiness Harness cannot be safely reattached
across an Agent crash.

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
serialized per Agent. Its request-independent owner retains the executor gate
and direct child handle until success/failure/timeout is durably terminal;
dropping that handle kills the direct child. Stale `running` state is converted
to a durable failure after an Agent restart because Nexus cannot attach to an
old child.
Update install and update-configuration Set/Clear share the same executor gate
from the idle check through the atomic config write, so configuration cannot
change underneath an active install.

The configuration API uses the same v1 protocol as the CLI. `nexusctl config
status` shows a redacted summary, while `nexusctl config clear-harness` and
`nexusctl config clear-update` remove one owned section after the corresponding
runtime has stopped/returned idle. Setting sections is currently an API-level
operation so a future Tauri, Electron, browser, or script frontend can supply
typed forms without taking ownership of persistence or process coordination.

`nexus-launcher` is the headless host/runtime boundary around the Agent. `api`
(also the no-argument mode) resolves the sibling `nexus-agent` (or an explicit
`--agent`/`NEXUS_AGENT_BIN`), creates a recoverable lock and launch record below
`run/`, redirects Agent logs into the Nexus `logs/` directory, waits for
loopback health, binds the Launcher API at `127.0.0.1:3091` by default, and
exposes local `/launcher/*` controls. It passes `NEXUS_CONSOLE_PORT` to a
newly spawned Agent for legacy browser clients, starts a configured Harness
after Agent is healthy, and watches Agent so a crash can be recovered without
another manual command. It has no static-file fallback and does not open a
browser on startup. Agent is always a separate operating-system process:
stopping the native shell or Launcher host does not implicitly stop Agent. Use
the UI's explicit stop action or `nexus-launcher stop` when Agent should end.
On POSIX systems, `start` and headless `api`/`console` launches put Agent in a
new process group before spawn, so Ctrl+C delivered to the Launcher's
foreground process group stops the API host without also signaling Agent.
The explicit `run`/`foreground` mode intentionally keeps Agent in the
foreground process group.

The Launcher also exposes `GET /launcher/harness` and
`POST /launcher/harness` with `{"action":"open"}`. These endpoints inspect
only bounded tails of the per-run log files named by the current durable
session marker. They first require a stable Agent-reported `running`
observation whose log-session identity matches the durable marker. Missing or
invalid markers, Agent failures, all non-running states, marker changes during
the read, files shorter than their recorded watermark, and a same-path file
whose identity changed fail closed with no URL/token. URL scanning retains raw
byte offsets even around invalid UTF-8. A candidate word must have observed
left and right delimiters, begin at or after the recorded EOF, and end after
it, so a URL split at the bounded-tail edge, incomplete at EOF, or crossing the
run boundary is rejected. An in-memory
per-file cursor/cache reuses a candidate only
when length and bounded-tail fingerprint are unchanged. Every detected content
or length change reparses the complete current tail, so a longer rotated file
cannot preserve a token that has scrolled out merely because its overlap matches
the previous file. The cursor is bounded to the latest 64 KiB per file; its safe
reconstruction boundary is the persistent session marker rather than cached
credentials. Valid loopback
HTTP URLs are selected by high-resolution file mtime, byte offset, and source
tie-breaks; URLs with remote hosts, HTTPS, credentials, or control characters
are ignored. The response includes the latest extracted token for local
display/copy in the native shell, but no arbitrary path or URL is opened and no
`.dsh`/Harness source is read.

`start`, `run`, `stop`, `status`, and `logs` remain script/recovery fallbacks.
They keep the existing guarantees: `start` returns only after health, `run`
keeps Agent attached in the foreground, and `stop` calls the Agent shutdown
endpoint and waits for the listener to disappear rather than killing an
arbitrary PID. Concurrent launchers contend on the OS lock; once its owner
exits, the same inode can be locked again without a delete/create race.

The native view lives under `apps/nexus-launcher/`. It presents overview,
Harness, profile, checkpoint, update, diagnostic, and settings modules with
loading, empty, and error states. Its Rust commands validate the API route
allowlist before proxying JSON to the headless Launcher and Agent. The UI keeps
Harness token metadata masked by default, embeds only validated loopback URLs,
and provides a system-browser action through the Launcher API. It derives token,
copy, open, reveal, and iframe state only when the independent Harness runtime
snapshot is `running` and `/launcher/harness` says the exact same generation
and run ID are available; every other or cross-generation combination unloads
the iframe and clears the visible credential. Reveal state is keyed to that
exact generation/run ID, so a fast restart cannot leave a new token in
plaintext. Stop/restart actions for either Harness or Agent synchronously
remove the token and iframe before native transport begins. That fail-closed
gate remains set after a failed request or endpoint error; only a positively
stopped runtime or a fresh matching generation/run ID can release it. Theme
selection supports System, Light, and Dark, persists locally, and follows
system preference changes when System is selected. Harness status is read from
`/launcher/agent-api/v1/harness`, while the URL/token view is read
independently from `/launcher/harness`. The UI polls every eight seconds when
stable and uses a
bounded roughly 400 ms loop while Harness is `starting`/recovering, without
overlapping refresh requests. No web product entry or static preview server is
required. If an action finishes while a refresh is already in flight, the UI
marks a pending refresh and drains it before the action is considered
refreshed, so the response reflects the resulting Harness state rather than
the pre-action snapshot.
The native proxy sends Launcher controls under `/launcher/*` and every Agent
read/write under the Launcher-only `/launcher/agent-api/v1/*` namespace to the
headless Launcher port; it never follows a bare `agent_api` advertisement and
never sends the Agent's native top-level `/v1/*` paths. The native
app generates a fresh helper nonce, passes it to the helper it starts, and only
after that nonce matches does it pin the returned Launcher data-root/instance
pair. A pre-existing process on the Launcher port is therefore unavailable,
even if it implements `/launcher/status`. Every subsequent native request
revalidates the pair. In addition, the native
app generates a fresh 256-bit capability and passes it only in the owned helper
process environment. The helper consumes and removes it before it can spawn an
Agent or Harness; it is never accepted as a CLI option or returned by status.
For every operation the native side opens one TCP connection, sends a fresh
public challenge to `/launcher/handshake`, and verifies an HMAC proof bound to
the pinned identity. Only after proof succeeds does it send the capability and
command body over that same connection. Every route other than bootstrap
status/handshake requires the capability and public identity pair. A process
that wins a port race can therefore receive a public challenge, but cannot
forge the proof or receive the secret/body on a replacement connection. That identity pin is
permanent for the helper nonce: a timeout or same-nonce identity mismatch marks
the helper unavailable but cannot erase or relearn the pin. The pin and nonce
are reset only after the native child handle confirms the old helper exited and
a new helper receives a fresh nonce. Helper verification checks the owned child
before and after the HTTP identity exchange, and proxy calls check it again
around the request. A missing child or failure to query its handle remains
fail-closed. The headless Launcher allows unauthenticated access only to the
bootstrap `/launcher/status` and public-challenge `/launcher/handshake`;
every operational `/launcher/*` route rejects a missing capability or
mismatched pair. The headless
API does not expose top-level `/v1/*`; it translates only the exact
identity-bound `/launcher/agent-api/v1/*` allowlist to Agent requests. Thus an
Agent that rebinds the console port between probe and request receives an
unknown Launcher-prefixed path rather than a valid Agent operation. If
bootstrap verification is unavailable, the UI clears its endpoint snapshot,
clears any prior Harness session/token view, renders only the bridge error,
disables controls, and sends no Agent API requests. A final helper identity
probe failure makes the native side unavailable without discarding its pin, so
an earlier successful probe cannot keep the UI available or authorize a new
identity after the helper dies or the port is rebound. If Harness is reported
`starting` without an attached PID, lifecycle controls remain disabled while
ownership is unresolved. A `running` observation without an attached PID is
identified as externally managed and likewise disables start, stop, and
restart controls that the Agent would reject. Missing, unknown, or transitional
Harness state also disables every lifecycle control. For each Agent request the
headless Launcher probes the configured Agent, validates the data-root
identity, and forwards the exact observed data-root/instance pair in
internal headers. Agent rejects a missing half-pair or any mismatch, closing a
port-rebind race before a state-changing request can reach a foreign Agent.

## Current scope and exclusions

This phase does not add Harness source dependencies, plugin marketplaces,
recommendations, advertising, cloud sync, remote control, or Launcher API
authentication. The Harness token viewer only
surfaces a token that the opaque upstream process already printed to a local
loopback URL; it is not a new Nexus authentication protocol. The single-entry
Rust host is available via `nexus-launcher api` (with `console` as a legacy
alias), while native packaging is under `apps/nexus-launcher/` against the
Launcher/Agent protocols. Embedded and external browser access is limited to
validated loopback origins on the configured API port. Release registration/promotion remain
explicit metadata operations;
the update executor installs a verified immutable slot but does not silently
change the active pointer or start Harness. Once a slot is explicitly promoted,
the supervisor resolves `{release_root}` from the catalog and performs launch
from that canonical directory; it never guesses a release from the process
working directory. Diagnostics are bounded and redacted as described above;
they are not a general filesystem archive. Profile selection does not claim to
understand Harness internals, and checkpoint restore remains metadata-only.
