# Nexus architecture baseline

Status: Phase 14 shared Agent runtime and direct Tauri API boundary

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

### Agent is the host; native GUI is replaceable

`nexus-agent` is the independent user-facing runtime boundary. It starts and
supervises Harness, owns the Nexus data root, and exposes the versioned
loopback JSON API. `nexus-launcher-core` contains UI-independent Agent HTTP
contracts plus the common Agent process resolver/lifecycle manager used by the
native shell and the legacy headless client. The historical `nexus-launcher`
`api`/`console` commands remain compatibility clients for scripts; they are not
required by the GUI.

`apps/nexus-launcher` is the native Tauri 2 shell. It owns navigation, window
state, tray behavior, theme preference, and safe presentation. Its Rust side
starts/probes the independent Agent through `nexus-launcher-core` and proxies a
fixed allowlist of Agent `/v1/*` routes; the React frontend never relies on
browser CORS or a custom-origin HTTP request. The shell can be replaced by
Electron, another native toolkit, or a script without changing Agent business
behavior. The stable cross-language boundary is the Agent HTTP/JSON contract,
not a direct Electron dependency on the Rust crate.

The embedded Harness Web view is optional and remains an opaque upstream
surface. Its URL is accepted only when the Agent has supplied a validated plain
HTTP loopback URL. A system-browser fallback uses the same native validation.

## Components

```text
nexus-protocol       versioned JSON wire types (v1)
       ^
nexus-core           paths, configuration, and state model
       ^
nexus-launcher-core  bounded Agent client + Agent process lifecycle contracts
       ^
nexus-agent          independent loopback HTTP server, HarnessSupervisor, updater, diagnostics, config
       ^
nexusctl             CLI client (`status`, `harness`, `profile`, `checkpoint`, `release`, `update`, `config`)
nexus-launcher       legacy headless compatibility client (`api`, `console` alias)
apps/nexus-launcher  native Tauri 2 shell with direct Agent loopback proxy
```

### Process model

These are separate processes with different responsibilities:

- `nexus-agent.exe` is the independent long-lived control-plane process. It
  listens on port 3090 and owns Nexus state and Harness supervision.
- `nexus-launcher-app.exe` is the replaceable native Tauri shell. It starts or
  probes `nexus-agent.exe` through the shared core, proxies local `/v1/*`
  requests through Rust, and owns only GUI, tray, notification, theme, and
  single-instance state.
- `nexus-launcher.exe` is a legacy long-lived headless compatibility client.
  It may use the shared core and preserve the historical Launcher API for old
  scripts, but the native app never spawns or requires it.
- `nexusctl.exe` is a short-lived command client. It sends HTTP requests to
  Agent and exits; it is not a daemon and does not host Agent.
- The configured Harness runtime is another process started and supervised by
  Agent. It remains an immutable upstream runtime.

Stopping the native shell or headless Launcher host does not implicitly stop
Agent. An explicit Launcher/UI stop request is required when the independent
Agent process should end.

Both Rust entry points use the same `nexus-launcher-core::AgentRuntime` for
Agent resolution and lifecycle. Startup takes the shared
`run/agent-bootstrap.lock`, rechecks the Agent's data-root identity, and lets
the Agent's `run/agent.lock` remain the process-lifetime owner; shutdown binds
the request to the exact identity observed immediately before the POST. The
Windows spawn path uses a no-console, new-process-group configuration so the
independent Agent does not open a second console window. The Tauri shell
attempts automatic Agent startup only once per session; refresh after an
explicit Stop is observational, while Start/Restart and the visible Retry
action are explicit lifecycle requests.

Release packaging runs the local, locked `nexus-agent` build through
`apps/nexus-launcher/src-tauri/scripts/prepare-agent.mjs` and bundles only the
resulting `resources/nexus-agent`, `resources/nexus-launcher`, and
`resources/nexusctl` executables. The Tauri resource map gives each staged
binary an empty target, placing all three at the bundle resource root (the same
install directory as the launcher executable on the supported Windows layout);
the resolver also accepts `<exe>/resources/nexus-agent` as a bounded platform
fallback. The staging script builds only those three local packages with
`--locked` and never downloads, starts, or edits Harness. An installed shell
therefore does not depend on a workspace `target/` directory, while
`NEXUS_AGENT_BIN` remains an explicit signed-binary override. The GUI does not
spawn or require `nexus-launcher` or `nexusctl`; they are legacy/headless
compatibility binaries bundled for existing scripts.

The Agent exposes:

- `GET /v1/health` — process health;
- `GET /v1/state` — current Agent/Harness state;
- `POST /v1/lifecycle` with `{"action":"shutdown"}` — graceful shutdown;
- `POST /v1/shutdown` — convenience graceful-shutdown endpoint.
- `GET /v1/harness` — current external Harness process information;
- `POST /v1/harness` with `{"action":"start|stop|restart"}` — explicit
  process control. `status` is also accepted as a harmless query action.
- `GET /v1/harness/ui` — a bounded, validated Harness loopback URL/token
  observation tied to the current Agent-owned generation and log session;
  unavailable or PID-less sessions fail closed.
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
current Harness Web port. No remote bind option is exposed in this phase. The
shared client preserves loopback-only transport, bounded JSON bodies, and
data-root/instance identity checks on requests. A separate local
authentication token remains a future protocol change; the identity contract
must remain in place before any remote bind is considered.

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
Invalid file values fail closed with the config path in the error. The legacy
headless Launcher continues to honor its console-port compatibility settings.
The native Tauri shell reads the Agent `NEXUS_DATA_DIR`/`NEXUS_AGENT_PORT`
configuration and fails closed on an invalid Agent health or identity response;
it never silently falls back to a Launcher port. Unknown JSON fields are
ignored for forward compatibility.

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

`nexus-agent` is the independent headless host/runtime boundary around
Harness. It binds the versioned API at `127.0.0.1:3090` by default, owns the
Nexus data root, starts and supervises the configured Harness, and keeps its
state usable without a GUI. `nexus-launcher-core` supplies the bounded Agent
client and common process resolver/start/probe/stop implementation. The
native Tauri shell uses those contracts directly, so it never starts a
headless Launcher helper.

`nexus-launcher` `api` (also its no-argument mode) remains a legacy script
compatibility client. It may resolve/spawn Agent through the shared core and
continues to expose its historical `/launcher/*` API for old headless/browser
clients. It has no role in the native app topology. On POSIX systems its
legacy Agent launches use a separate process group; the explicit `run`/
`foreground` mode intentionally keeps Agent in the foreground group.

The legacy Launcher also retains `GET|POST /launcher/harness` with
`{"action":"open"}` for existing scripts. That compatibility path now calls
the same shared bounded parser used by `GET /v1/harness/ui`; it remains
available for old scripts but is not part of the native app contract. The
direct Agent `/v1/*` API is the only GUI transport.

`start`, `run`, `stop`, `status`, and `logs` remain legacy script/recovery
fallbacks. They keep the existing guarantees: `start` returns only after
health, `run` keeps Agent attached in the foreground, and `stop` calls the
Agent shutdown endpoint and waits for the listener to disappear rather than
killing an arbitrary PID. Concurrent legacy launchers contend on the OS lock;
once its owner exits, the same inode can be locked again without a delete/create
race.

The native view lives under `apps/nexus-launcher/`. It presents overview,
Harness, profile, checkpoint, update, diagnostic, and settings modules with
loading, empty, and error states. Its Rust commands validate the shared
`/v1/*` Agent route allowlist before proxying bounded JSON to Agent. Startup
status comes from `AgentRuntime` and direct `GET /v1/health`; AppState contains
no helper process, resource directory, capability, or Launcher identity.

The UI keeps Harness token metadata masked by default and embeds only validated
loopback URLs. The Agent owns the bounded `/v1/harness/ui` URL/token response;
the native bridge handles the system-browser action after the same loopback
validation. Stop/restart actions for either Harness or Agent synchronously
remove the token and iframe before native transport begins. Theme selection
supports System, Light, and Dark, persists locally, and follows system
preference changes when System is selected. The UI polls every eight seconds
when stable and uses a bounded roughly 400 ms loop while Harness is
`starting`/recovering, without overlapping refresh requests. No web product
entry or static preview server is required. If an action finishes while a
refresh is already in flight, the UI marks a pending refresh and drains it
before the action is considered refreshed, so the response reflects the
resulting state rather than the pre-action snapshot.
The native proxy sends only the documented Agent `/v1/*` routes to the Agent
port. The core validates method/path/body bounds, probes `/v1/health`, and
binds the observed data-root/instance identity to subsequent requests. A
port-rebind or unrelated loopback listener therefore fails closed rather than
receiving a state-changing request. The `/v1/agent` route is a small native
lifecycle adapter for start/stop/restart/status; all business state and
Harness operations go directly to Agent. The native process does not carry a
helper nonce, capability, HMAC handshake, helper resource, or Launcher proxy
identity.

The legacy headless Launcher keeps its historical `/launcher/*` and
`/launcher/agent-api/v1/*` compatibility surface for old scripts. Its
legacy-specific lock, capability, and log-session guarantees remain relevant
only to that compatibility process and are not a dependency of the Tauri app.
If Agent bootstrap verification is unavailable, the native UI clears its
endpoint snapshot and Harness session/token view, renders only the bridge
error, disables controls, and sends no Agent API requests. Missing,
unknown, or transitional Harness state likewise disables lifecycle controls.

## Current scope and exclusions

This phase does not add Harness source dependencies, plugin marketplaces,
recommendations, advertising, cloud sync, remote control, or a new Agent
authentication protocol. It adds `nexus-launcher-core` as a UI-independent
Rust bridge for the stable Agent HTTP/JSON contract and common Agent process
lifecycle. The Agent `/v1/harness/ui` endpoint only surfaces a token that the
opaque upstream process already printed to a local loopback URL; it does not
introduce a new credential source. The single-entry Rust host is
available via `nexus-launcher api` (with `console` as a legacy alias), while
native packaging is under `apps/nexus-launcher/` directly against Agent. Future
Electron integration consumes the same Agent JSON API and does not link the
Rust bridge directly. Embedded and external browser access is limited to
validated loopback origins on the configured API port. Release registration/promotion remain
explicit metadata operations;
the update executor installs a verified immutable slot but does not silently
change the active pointer or start Harness. Once a slot is explicitly promoted,
the supervisor resolves `{release_root}` from the catalog and performs launch
from that canonical directory; it never guesses a release from the process
working directory. Diagnostics are bounded and redacted as described above;
they are not a general filesystem archive. Profile selection does not claim to
understand Harness internals, and checkpoint restore remains metadata-only.
