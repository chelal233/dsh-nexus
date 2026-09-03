# Nexus architecture baseline

Status: Phase 1 bootstrap

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

The initial Agent state is intentionally small: it reports its own lifecycle
and whether a Harness process is attached. Harness process management will be
added behind the same Agent boundary in later phases.

### Console/WebShell is replaceable

Tauri is not a business layer. A future Tauri Console/WebShell may provide
windows, tray behavior, notifications, and a Harness Web view, but it must
consume the Agent protocol rather than own lifecycle state or write runtime
files directly. Electron, a browser, or a script can replace it without
changing Agent behavior.

## Phase 1 components

```text
nexus-protocol  versioned JSON wire types (v1)
       ^
nexus-core      paths, configuration, and state model
       ^
nexus-agent     foreground loopback HTTP server
       ^
nexusctl        CLI client (`status`, optional JSON output)
```

The Agent exposes:

- `GET /v1/health` — process health;
- `GET /v1/state` — current Agent/Harness state;
- `POST /v1/lifecycle` with `{"action":"shutdown"}` — graceful shutdown;
- `POST /v1/shutdown` — convenience graceful-shutdown endpoint.

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
`releases`, `downloads`, and `run`. It does not copy credentials, Harness
sessions, or secret environment values into Nexus state.

## Current scope and exclusions

This phase does not add Tauri, Electron, Harness source dependencies, plugin
marketplaces, recommendations, advertising, cloud sync, or remote control.
Those can be considered later without changing the Agent-first boundary.
