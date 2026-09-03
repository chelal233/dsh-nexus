# Launcher hardening report

## Scope

- Task: `nexus-bootstrap`
- Phase: `launcher-hardening`
- Base: `4b8870b3ce5c8023026db774b4b66ffbcd0c98bb`
- Worktree: `E:\git\dsh-nexus-launcher-hardening`
- Changed paths: `crates/nexus-launcher/src/main.rs`,
  `docs/architecture-baseline.md`, and this report.

## Delivered

- Agent startup records the existing Agent stdout length and accepts the new
  `nexus agent listening` marker as a same-process readiness fallback when a
  concurrent local client causes transient Windows TCP refusals. HTTP health is
  still the first-class probe whenever it is available.
- Foreground mode uses non-blocking child polling and a bounded Ctrl+C shutdown
  loop. When `stop` has removed the launch metadata, it does not await a
  platform process notification indefinitely; any fallback targets only the
  owned child handle.
- The architecture baseline documents these recovery boundaries and keeps
  Harness and `$HOME/.dsh` outside launcher ownership.

## Verification

- `cargo fmt --all -- --check` — pass.
- `cargo test --workspace --locked` — 33 passed, 0 failed.
- `cargo build --workspace --release --locked` — pass.
- Windows PowerShell sequence with detached start/stop followed by foreground
  run, concurrent early health polling, graceful stop, and redirected output —
  pass (`manual-sequence-debug.ps1`).
- Full release lifecycle/WebShell smoke on the merged main binary — pass
  (`launcher-smoke.ps1`, dynamically allocated loopback ports).
- `git diff --check` — pass.

## Boundaries

No Harness source, package metadata, build files, `.dsh` data, production data,
or deployment target was changed. Native Tauri/Electron packaging and browser
CORS remain separate follow-up work.
