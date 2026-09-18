# Troubleshooting

[简体中文](troubleshooting.md)


Record the Nexus version, build ID, OS, and architecture, and retain the original error. These steps do not require deleting Harness user data.

| Symptom | Check and response |
| --- | --- |
| `Resolve the blocking startup checks…` | Inspect the actual `blocked` items; this is a summary, not the cause |
| `configured_path_missing` or missing Node launch program | Compare next launch inputs with the current extracted location; inspect `resources/runtime` and stale pins |
| `ENOENT … app-update.yml` | Use a complete release package with the file under `resources`, not a copied EXE. The inspected v0.1.7 Windows x64 ZIP and installer both contain it |
| Saved runtime still launches an old path | Known in v0.1.7, fixed in current source but not yet in that package. A workaround must check both runtime settings and the advanced launch program |
| JSON configuration parse failure | Identify the file and line/column, preserve a backup, and use guided repair or restore a valid configuration; do not reset all directories |
| Plugin Loader failure | Preserve the stack, run compatibility checks, and check plugin provenance/declarations and Nexus/Harness versions; do not assume every failure is a user plugin defect |
| Interrupted operation awaiting recovery/cleanup | Retry recovery/cancellation through the interface; do not delete markers to bypass process ownership checks |
| Missing verified rollback target | Complete authenticated readiness for the current version, or follow an explicitly offered manual confirmation path; a version pointer is not health evidence |
| No notifications | Check categories, conditions, observer connection, OS permissions and do-not-disturb; terminal delivery requires its listener to remain open |

## Portable relocation

v0.1.7 can retain an automatically selected bundled runtime as an old absolute path. The source fix re-resolves `bundled` selections against the current installation and updates the linked launch program. Explicit external paths remain untouched. If an old save marked the selection external, it requires an explicit correction rather than guessing from the directory name.

## Diagnostics and recovery

Launcher startup diagnostics may remain available without the Agent. Recovery mode pauses Harness startup; repair, recheck, then start explicitly. Legacy records without recoverable process identity may require one computer reboot as directed. Ordinary path configuration errors should not require rebooting the computer.

Report a minimal reproduction, original error, version, build ID, and redacted screenshots. Inspect diagnostics before sharing; do not publish tokens, API keys, private conversations, or full configurations. See [security reporting](../SECURITY.en.md).
