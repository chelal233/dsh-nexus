# Nexus Console / WebShell

This directory is a dependency-free browser client for the loopback Nexus
Agent. It is intentionally not a second business layer: all state changes go
through the versioned `/v1/*` API, and all sensitive configuration values are
redacted by the Agent before they reach the page.

## Preview

Build the workspace first, start the Agent with `nexus-launcher start`, then
serve this directory from the fixed local origin used by the future UI shell:

```text
python -m http.server 3091 --directory apps/nexus-console
```

Open <http://127.0.0.1:3091/> and leave the Agent API field at
`http://127.0.0.1:3090` (or enter another loopback port). A native Tauri or
Electron shell can load the same files or replace them entirely; it must keep
the Agent as the source of truth.

The Agent's browser-origin CORS policy is deliberately a separate integration
slice. Until that policy is enabled, use the page as a UI/static review or
load it from the eventual native shell; do not weaken the loopback boundary by
opening the Agent to remote origins.
