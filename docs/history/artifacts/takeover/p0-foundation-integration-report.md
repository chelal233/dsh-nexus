# P0 foundation isolated integration

This integration establishes the base for the remaining P0 implementation. It
does not mark P0 complete and does not merge into the original main checkout.

## Provenance and authorization

- Runtime base: `8261a6e35c485a85c7b4e203c682bb956d52e62a`.
- Runtime correction and integration parent:
  `a98beb890de8119e7a90f4e8f6f976da13faa34a`.
- Snapshot source: `0b8fb8110b921ba22122270338b89bd1e25f2536` plus correction
  `9875c84c9d7e86523ed4acb2651453c56452829e`.
- Worktree: `E:/git/dsh-nexus-phases/p0-integration`.
- Branch: `codex/nexus-p0/integration`.
- Kernel: `E:/git/dsh-nexus-phases/p0-integration-state.json`.

The user previously authorized continuing isolated integration using existing
independent review and test evidence. That authorization is applied here; no
HOST_VERIFIED telemetry is fabricated. The snapshot review recommends merge
with human review because persistent state is involved. This is an explicitly
authorized local integration, not an automatic production merge.

Both corrected heads received independent PASS:

- `E:/git/dsh-nexus-phases/reviews/p0-runtime-a98beb8-review.md`.
- `E:/git/dsh-nexus-phases/reviews/p0-snapshot-9875c84-review.md`.

The full corrected snapshot branch was integrated with `git merge --squash`
into the runtime correction, producing one direct-parent phase commit. Original
branches and source commits remain intact. All eight snapshot paths match
`9875c84` exactly; runtime implementation remains identical to `a98beb8`.
Only this report and project/architecture status documentation are added beyond
those reviewed implementation blobs.

## Verification

- `cargo test --offline -p nexus-agent -p nexus-core -p nexus-protocol -p
  nexus-launcher-core -p nexus-snapshots`: exit 0; Agent 101, Core 30,
  Protocol 13, Launcher Core 11, Snapshots 17; total 172. All doc-tests passed.
- `cargo check --offline -p nexus-cli`: exit 0.
- `git diff --check` and staged diff check: passed.
- Logs: `E:/git/dsh-nexus-phases/reviews/p0-foundation-integration-rust.log`
  and `p0-foundation-integration-cli.log` in the same directory.
- The original main remains `df3cff00cb5e740f223897756f2c7f5bf9c44207`; all
  five pre-existing dirty-file hashes matched the saved takeover fingerprint.

## Remaining P0

Runtime supply/confirmation, real cold clone/install/build and Node configuration,
snapshot integration with the existing Agent journal and healthy-start hook,
dependency materialization, official plugin/native Profile adapters, four recovery
tabs, and real Node/native GUI acceptance are still required. The standalone
snapshot crate is not yet wired into Agent checkpoint behavior.

System installation remains a real product requirement with mock-runner/source
verification here; real workstation tests remain portable-only. Real Windows
junctions and injected interruption points were tested. Physical power loss,
system installation, real Harness launch and GUI were not tested by this phase.
Manual capture can leave unidentified staging content that fails closed and
requires explicit cleanup; the recovery surface must provide actionable guidance.
