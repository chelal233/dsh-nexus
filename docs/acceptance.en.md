# Manual acceptance checklist

[简体中文](acceptance.md)


Record version, commit, build ID, platform/architecture, package type, and data roots. For each scenario record actual results, evidence, and gaps; old reports cannot accept a new build. Use isolated data for forced termination, migration, and recovery.

| Scenario | Acceptance condition |
| --- | --- |
| Clean install / ZIP extraction | Opens without development tools; resources and Agent identity are correct |
| Portable relocation | Bundled runtimes use the current directory; external pins stay unchanged; user data is not migrated |
| Runtime saves | Running Harness unchanged; next inputs refresh immediately; stop/start uses new settings without restarting Nexus |
| Arguments, data directory, profile | Save/clear/conflict handling is correct; next inputs match the next process; current evidence is not relabeled |
| Version preparation/switch | Preparation does not force a switch; stop/confirmation is clear; failures remain understandable |
| User data | Compare profiles, sessions, and projects before/after; probes leave no user-visible test profiles |
| Plugins | Market and no-market paths work; declaration conflicts explain themselves; exercise real business functions |
| Notifications | Completion/failure/approval/question, unfocused/always, foreground Launcher with background task, click-through |
| Interruption recovery | Kill at authorized test points; restart does not replay uncertain actions, orphan owned processes, or delete business data |
| Nexus update | Auto/manual checks, background download, interruption, superseding versions, clear Harness-stop warning before applying |
| System integration | Windows/macOS/Linux pickers, terminal, clipboard, IME, zoom, tray, sleep/resume |
| Official Desktop | Supported targets expose the option; unsupported targets hide it; stop before switching modes; preparation cancellation and failure recovery preserve data |
| Full offline package | Export/import without network on matching OS/architecture; reject mismatches; retain Unix permissions/links and macOS signed app structure |
| Tray | Web/Desktop controls, configuration and maintenance entries; exit-only retains services, stop-all exits only after stopping |
| Client startup | Required service failures block success; optional warnings remain limited; absent reports stay unverified; provider evidence precedes disable suggestions |
| Languages | Chinese/English actions, errors, confirmations, long text, and screenshots |
| Uninstall | Respects selected scope; preserves external directories and unauthorized data |

CI smoke is not a PASS for this entire table. Report only scenarios actually exercised. Source fixes require repackaging before installed-build acceptance.
