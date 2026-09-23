# Configuration and when it takes effect

[简体中文](configuration.md)


This page uses v0.1.8 as its baseline. Runtime-path and save fixes shipped in that version; v0.1.7 may still be affected. See the [changelog](../CHANGELOG.en.md).

## Saved settings, next launch, current instance

| Setting | After saving | Effect on running Harness |
| --- | --- | --- |
| Node, pnpm, Git selection | Subsequent starts and dependency operations use it; refresh runtime checks manually | Does not replace an existing process |
| Harness arguments, data directory, profile, patches | Used on subsequent starts; some actions require Harness stopped | No live replacement inside the process |
| Harness update source | Used for subsequent fetch/preparation; finish active updates first | Does not change the current version |
| Snapshot settings | Read by subsequent snapshot operations; active operations remain protected | Does not modify the current session |
| Notification categories/conditions | Read by the notification pipeline after saving | Observer loading changes require a Harness restart |
| Theme, language, zoom | Applied to the current Launcher | Does not alter Harness settings |
| Automatic updates | Controls subsequent automatic checks; manual checks are separate | Harness stops only when applying an update |
| Agent log level | Used on the next Agent start | Does not reconfigure a running Agent |

Next launch inputs describes resolved configuration. Current instance inputs records what that instance actually started with. A difference is not necessarily a defect. Ordinary saved settings must not require restarting all of Nexus to update the next Harness launch.

![Settings](images/settings-en.jpg)

## Bundled and external runtimes

The development version bundles Node/npm/pnpm and a complete Git command line. Explicitly configured tool paths take priority. npm belongs to the selected Node distribution. Missing selected tools cause an error instead of silently using host PATH tools. Full Git travels with offline exports; older archives without Git use the current Nexus bundle. This change is not yet released and does not describe the existing 0.1.10 installers.

Current source resolves `bundled` runtimes relative to the current installation and preserves explicit external selections. Earlier versions may retain automatically generated absolute paths. Directory names alone cannot establish whether a user deliberately pinned a path.

Saving runtime settings keeps an automatically generated Node launch entry linked to the runtime. A separately specified launch program retains its own precedence. Clearing runtime pins does not clear an independently configured custom program.

## Environment overrides and conflicts

Environment variables may override saved file settings; the interface indicates relevant overrides. Processes inherit environment variables at creation, so changing a parent environment cannot update an existing child. This does not mean ordinary settings require restarting Nexus.

Configuration saves check revisions. If another operation changed the document, retain the draft, refresh, and retry rather than overwriting newer content. Runtime, version-switch, and recovery operations still respect mutual exclusion.

## Data and patches

Changing data directories does not migrate files. Patches are composed through the selected Harness's supported mechanism; order and enablement affect the result. Launch input descriptions are not the entire upstream merged configuration. Do not publish secrets from diagnostic, patch, or configuration files.

## Web and official Desktop

Switch Web profiles from Workbench or Profiles and plugins. Official Desktop manages configuration in its own window. The modes share managed versions and data: stop the active mode before switching modes, changing a version, or exporting data. Desktop uses its pinned offline runtime; Web runtime settings are not an arbitrary Desktop runtime override. See [Desktop](harness-desktop.en.md).

## Plugin management while Harness is stopped

When the selected Harness includes its official plugin manager, Nexus uses that version's `listBundles`, `setBundleEnabled`, `inspect`, `installBundle`, and `removeBundle` implementation. The selected profile must be stopped before changes. Management-required components retain upstream protection. Disabling retains installed dependencies; new installations remain disabled until explicitly enabled. No user plugin is loaded for these operations. Unsupported older Harness versions retain the legacy interface; a failed official manager does not silently fall back to legacy mutations. Package-script approval remains in Harness; Nexus does not auto-approve scripts.

Snapshots and checkpoints show local-time titles. Details open in a side drawer with readable fields by default and a Source code switch.

## Offline profile repair

Choose a target under Maintenance → Offline profile repair without selecting it as active or starting its plugins. Damaged profiles remain visible. Stop Web, Desktop and DSH terminals first.

Inspect/edit `package.json` and `cordis.patch.yml` (32 KiB per file; oversized files are rejected, never truncated). Saving validates JSON/YAML and basic structure, then reads the file back. Every save/restore preserves the exact previous file; official plugin enable/disable/remove/install also backs up both configuration files first. Recovery points restore configuration only, not removed plugin dependencies. A restored original may still be invalid; diagnostics remain visible.

Concurrent file changes reject stale writes. Use local dependency repair for missing packages and official plugin controls for plugin errors; do not infer the culprit from missing service names alone. Format checks are not compatibility or startup acceptance. Verify the relevant mode from the Workbench. No automatic Harness restart or safety-mode profile is created.

## Start first, diagnose on failure

Normal Web and Desktop launches do not pre-run isolated plugin diagnostics, copy profiles for diagnostics, or scan the dependency tree. Essential runtime, configuration and process ownership checks remain, as does the official Desktop artifact preparation required for its first launch. Browser access still waits for client readiness.

Failed launches retain their current evidence. Only explicit missing-module or import failures trigger a local dependency check. Missing links are restored only when the lockfile and local package identity agree, with a repair record and at most one retry. This does not download packages, replace existing links, disable plugins or alter configuration. Stop, release changes and configuration changes cancel recovery. Other plugin errors and timeouts remain available for user-directed diagnostics. Full compatibility checks remain available manually.
