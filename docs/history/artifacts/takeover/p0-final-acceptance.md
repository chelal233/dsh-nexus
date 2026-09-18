> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../../README.md) · [当前文档 / Current documentation](../../../README.md)

# P0 final acceptance — 2026-09-05

Status: COMPLETE within the authorized isolated integration scope. P1/P2 have not started. No main merge or deployment was performed.

Implementation head: `73dec3763d2e33775ec8511b94a6f7c08404d53a`, branch `codex/nexus-p0/integration`. The final report commit changes documentation only.

## Verification

- Combined offline Rust suite: 220 passed, 0 failed (Agent 130, Core 31, Protocol 13, Launcher Core 11, Snapshots 17, Runtime Supply 18). CLI compiled and its zero-test target passed. Frontend: 22 tests, typecheck and production build passed. Final Tauri debug/no-bundle build passed with refreshed Agent/CLI resources.
- Real cold operation `cold-1788605862320217700` cloned official `dsh-v0.1.2-alpha.3`, confirmed the plan, reused verified Node 24.19.0 and exact cached pnpm 11.7.0, installed frozen dependencies, built, verified the CLI, registered and promoted. Harness remained stopped until explicitly started. Installed-release fast switching also passed without another clone.
- Real isolated Harness start and authenticated HTTP 200 passed. The final native system-browser button opened a browser. The user confirmed that the browser worked and Harness had started. This final browser visual acceptance is user-confirmed; automation stopped when its URL policy check could not identify the browser URL reliably.
- Healthy/manual checkpoints passed. Actual restore removed a file originally absent, restored profile patch bytes exactly, and restored package metadata using actual pinned pnpm materialization. No pending checkpoint remained.
- Native profile inventory/selection passed. Built-in plugin removal returned 403. A local offline fixture plugin was removed through the real official CLI with exit code 0 and `removed: true`.
- Existing independent reviews and targeted fixes cover supply integrity/publication, snapshot recovery, cold-operation ownership/cancellation, diagnostics and UI gates. Crash/cancellation and system-installer branches use fault injection or mock runners, not actual power cuts or system installation.

## Evidence and boundaries

Evidence directory: `E:/git/dsh-nexus-phases/acceptance/p0-60c0faf`. Files include `final-rust-tests.log`, `native-build-final.log`, `cold-status-gitpin.json`, `installed-fast-switch.json`, `manual-restored.json`, `materialization-restore.json`, `local-plugin-remove-final.json`, and `final-stop.json`.

Executable: `E:/git/dsh-nexus-phases/p0-integration/apps/nexus-launcher/src-tauri/target/debug/nexus-launcher-app.exe`.

Authenticated Web uses a top-level browser; authenticated iframe embedding is not claimed. Actual runtime acquisition exercised reuse, not fresh Node/pnpm artifact download. MSI/script system installation and Unix runtime behavior were not exercised. No real user DSH data, global PATH/Git configuration, or upstream source was modified.

The task-owned native app, Harness and Agent were stopped; their PIDs and listeners on 31397/3080 were absent afterward. Fixture data is retained. Main remains `df3cff00cb5e740f223897756f2c7f5bf9c44207`; all five previously recorded file hashes remain unchanged. Main integration and deployment remain separate work.
