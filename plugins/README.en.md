# Bundled Nexus plugins

[简体中文](README.md)


[Documentation](../docs/README.en.md)

## Modules and ownership

Each feature lives in `plugins/<plugin-name>/` with its own manifest, host/client entry points and tests. Launcher/Agent owns deployment, lifecycle, authenticated communication and native capabilities. Each plugin interprets its own business events. Cordis composition loads plugins without modifying upstream source.

| Plugin | Responsibility |
| --- | --- |
| `nexus-notifications` | Task events and terminal alerts. |
| `nexus-desktop-bridge` | Window health, native directory selection, notification session routing and read-only `desktopWindow`. Electron preload exposes real dropped-file paths through `__DSH_DESKTOP_FILE_PATH__`. |
| `nexus-desktop-compat` | `desktopProfiles.current/list/select` and `desktopPnpm.run/runPlugin/runExternalMarketPluginInstall`. Agent owns profile switching; Harness subprocess services own package operations until the process tree exits. |

## Loading and compatibility

Loader patch `name` values must be JSON strings containing escaped `file://` module URLs. Do not serialize Rust `OsString` or pass raw Windows drive paths. CLI `entry` and `pnpm` fields use ordinary path strings. `generated_plugin_json_uses_string_paths_and_loads_in_upstream_loader` checks generated files; with `NEXUS_TEST_DSH_ROOT`, it also loads all three plugins through the actual Harness loader.

The compatibility layer does not bundle upstream desktop runtime modules or promise private APIs. Plugins should use service injection and capability checks. Nexus uses the actual Node ABI; ordinary `runPlugin` retains sources supported by Harness, including GitHub and local packages. Diagnostic probes cannot mutate packages or switch profiles.

When `dshmarket` is selected, a temporary patch declares its `desktopProfiles` dependency to avoid an incorrect self-restart branch caused by load order. User market configuration and upstream packages remain unchanged.

## Deployment

Agent carries the plugins, deploys them to Nexus-managed runtime directories and mounts them with `--patch` when Harness starts. This is not npm installation and does not write to user profile dependency manifests or `node_modules`. Disabling the notification plugin takes effect at the next Harness start. Market selection is optional; Nexus does not operate its own package registry.
