# P0 runtime and recovery Launcher UI report

Date: 2026-09-05
Base: `60c0faf18adc795a280b2cbe9be85cc2c5bb7484`
Branch: `codex/nexus-p0/integration`

## Result

The existing native Launcher now exposes the reviewed P0 runtime and recovery
contracts through the shared Tauri `proxy_request` bridge. It reuses the
existing visual system and components.

Updates preserve and display current Node, pnpm, and Git pins while saving
`official` or `npmmirror` source and `portable` or `system` mode through
`set_runtime`. Tag switch sends those preferences to the asynchronous cold
operation. Status includes phase, progress, identity, actionable errors,
refresh, and cancellation. Awaiting confirmation displays the exact plan token,
versions, destination, dispositions, artifact kinds, and system/portable effect
warning before explicit confirm or cancel. Success does not start Harness.

Recovery is an explicit sidebar module with Plugins, Rollback, Native profiles,
and Diagnostics tabs. It reads `/v1/recovery` independently of Harness health.
Profile, plugin, and restore mutations are disabled until Harness is positively
stopped/unowned according to returned recovery state; diagnostics and bounded
redacted log tails remain readable. Only valid manifest-backed profiles can be
selected. Plugin inventory distinguishes built-in/removable/protected entries;
official removal requires exact profile/package confirmation. Profile creation
and deletion remain excluded.

Checkpoint UI distinguishes legacy metadata-only entries, manual checkpoints,
and healthy/manual snapshot inventory. It exposes summary, detail, inspection,
redacted fields, omitted reasons, bounded content, and truncation notes. Pending
restore state displays its error and advertised Retry/Abort actions. Healthy
capture errors remain visible.

Local detail, tag, and plugin requests use request generations so older
completions cannot replace newer or cancelled view state. English and Simplified
Chinese dictionaries have exact key parity.

## Verification

- Launcher `typecheck`: passed with the requested cached Node/pnpm toolchain.
- Launcher tests: 20 passed, covering request races, stopped gates, recovery
  tabs, plugin classification, cold plan rendering, checkpoint state, and locale
  parity.
- Launcher production build: passed; Vite transformed 4,575 modules.
- `git diff --check`: passed.

No backend, protocol, DSH home, Harness, network, Corepack, installer, system
runtime, PATH, deployment, or real GUI interaction was used. Real native
interaction acceptance remains with the root acceptance phase.
