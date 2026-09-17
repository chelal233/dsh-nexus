# Switch ownership phase report

- Task / phase: `nexus-takeover` / `switch-ownership`
- Status: PASS, ready for controller review; not merged or deployed
- Base / branch: `fc0c5d1409e0e9476d365b641f1e1520fda470e8` / `codex/nexus-takeover/switch-ownership`

## Changed paths

- `crates/nexus-agent/src/lib.rs`
- `crates/nexus-agent/src/updater.rs`
- `artifacts/takeover/switch-ownership-report.md`

## Result

- Explicit Switch now acquires locks in the fixed order supervisor lifecycle then nonblocking updater gate, and transfers both gates to a detached owner before the update configuration, install, promotion, or terminal state is mutated.
- The detached owner retains lifecycle and updater ownership through release promotion, updater terminal persistence, and Agent current-release synchronization. Cancelling the HTTP handler no longer cancels the owner or releases either gate early.
- Cold Switch publishes `Succeeded` only after promotion. Command, promotion, promoted-catalog load, and Agent current-release synchronization failures publish `Failed` and return an error. A completed promotion is not rolled back when later synchronization fails, so the release pointer remains the source of truth while the failed update state exposes the partial finalization.
- Ordinary Install retains its prior detached owner and remains install-only. Fast Switch, tag validation, and missing-update-configuration behavior remain covered.

## Verification

- RED: `cargo test --offline -p nexus-agent cancelled_switch_keeps_lifecycle_and_update_gate_with_detached_owner -- --nocapture` exited 1 on the original behavior because the observed Harness start did not wait on the lifecycle gate (`RecvError`).
- Focused: `cargo test --offline -p nexus-agent switch_ -- --nocapture` exited 0; 5 passed. Deterministic oneshot coverage includes lifecycle waiting, request cancellation, second update conflict, SetUpdate/ClearUpdate try-gate conflict, SetHarness lifecycle waiting, command failure, promotion failure, no premature success, failure visibility, and gate release.
- Full Agent suite: `cargo test --offline -p nexus-agent` exited 0; 78 passed, 0 failed; doc-tests passed.
- Existing `updater::tests::cancelled_install_keeps_owner_gate_until_child_is_reaped_and_terminal` passed in the full suite, retaining the existing child-reap coverage around `run_logged_command`.
- `cargo fmt --all -- --check` remains exit 1 because the fixed base already has formatting diffs in out-of-scope files and pre-existing regions; no out-of-scope formatting rewrite was made.

## Not done

- No real Harness process, network clone, runtime installation, Node build-to-launch, native GUI acceptance, merge, or deployment was performed.
- The cold path was exercised with a local command fixture only; this phase does not claim real Node cold-install acceptance.
- Review package: None; the controller owns independent committed-diff review and package generation.
