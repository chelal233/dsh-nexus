> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../../README.md) · [当前文档 / Current documentation](../../../README.md)

# Nexus takeover isolated integration report

- Task / phase: `nexus-takeover` / `integration`
- Status: PASS; reviewable isolated branch prepared for controller review
- Base: `df3cff00cb5e740f223897756f2c7f5bf9c44207`
- Branch / worktree: `codex/nexus-takeover/integration` / `<WORKSPACE>/dsh-nexus-phases\integration`
- Identity evidence: `HOST_ACCEPTED`; no resolved model/effort telemetry was fabricated
- Scope: the 16 paths recorded in `<WORKSPACE>/dsh-nexus-phases\integration-state.json`

## Cherry-pick provenance

All commits were cherry-picked in the listed order. No cherry-pick conflict occurred.

| Reviewed source commit | Integration commit | Result |
| --- | --- | --- |
| `fc0c5d1409e0e9476d365b641f1e1520fda470e8` | `8e8b84745851cec130a0ab8f872445677c9d0370` | runtime discovery |
| `69d508493102c1460adb679f34aa89c9acebb630` | `5f0849862fbaace0ae90a607d1313fcb834d1229` | runtime hardening |
| `4b2e767f294c6c3268a6c63fdbdf3cd5a9643338` | `fc6bd7c99f7ea7215eef3c38bb3ffcdf5c5adcbf` | whole-request budget |
| `c9ae469668c649077241c323948306b52d20b431` | `bc773f70e3effa5a7ec69c5752e13fe00669ce96` | architecture handoff |
| `3297bdbfeda10b61be9c373def5c452ea5953c00` | `c75cb1bdfe90a03307939bee7ae17fc1a9c671f2` | architecture correction |
| `809dda1f6b327449e09b4cee5bbbf7dcba5e58a6` | `a1c30eab41d5f0d477b7bc35284b609bae3ca740` | manual runtime panel |
| `39d085b2b96a8e33080c1cf980a5b8e9f42483be` | `fa52119410eeba0ace18f51c859f5ccb014ed448` | strict UI parsing/state |
| `52962dbeb6bff16d5d786337afc956d04cb940c1` | `59c93231d6587a1c3aa9f5a3ff7fac22cc0937e4` | Switch ownership/finalization |

The original phase reports remain unchanged historical evidence. Their independent-branch results are not represented as combined integration acceptance.

## Integrated behavior

- Agent exposes read-only `GET /v1/runtime`; shared runtime and release-tag routes accept bodyless GET only.
- Runtime observation uses one six-second absolute request budget across configuration, enumeration, candidate preparation, process work, and cleanup. A global three-permit blocking owner bounds synchronous filesystem work, and detached ownership keeps child cleanup bounded after caller cancellation.
- The native Settings runtime panel performs only manual requests, strictly parses the fixed tool set and metadata, clears stale success on failure, and renders bilingual source labels.
- Explicit Switch owns the supervisor lifecycle gate and updater gate in a detached task through update configuration, install, promotion, durable terminal state, and Agent current-release synchronization. Success is published only after promotion; final synchronization failure is published as `Failed` without rolling back the already-promoted release pointer.

## Fresh integrated verification

| Command | Exit | Result | Log |
| --- | ---: | --- | --- |
| `cargo test --offline -p nexus-agent -p nexus-protocol -p nexus-launcher-core` | 0 | Agent 93/93, launcher-core 11/11, protocol 11/11; all three doc-test targets 0/0 | `<WORKSPACE>/dsh-nexus-phases\reviews\integration-rust-combined.log` |
| `COREPACK_ENABLE_NETWORK=0 pnpm install --offline --frozen-lockfile --ignore-scripts` from `apps/nexus-launcher` | 0 | lockfile accepted; 73 packages reused, 0 downloaded | `<WORKSPACE>/dsh-nexus-phases\reviews\integration-pnpm-install.log` |
| `pnpm typecheck` from `apps/nexus-launcher` | 0 | TypeScript check passed | `<WORKSPACE>/dsh-nexus-phases\reviews\integration-frontend-typecheck.log` |
| `pnpm test` from `apps/nexus-launcher` | 0 | 14/14 passed | `<WORKSPACE>/dsh-nexus-phases\reviews\integration-frontend-test.log` |
| `CARGO_NET_OFFLINE=true node apps/nexus-launcher/src-tauri/scripts/prepare-agent.mjs` | 0 | current integration source built release Agent/Launcher/CLI into ignored Tauri resources | `<WORKSPACE>/dsh-nexus-phases\reviews\integration-prepare-agent.log` |
| `cargo test --offline --manifest-path apps/nexus-launcher/src-tauri/Cargo.toml` | 0 | 7/7 passed | `<WORKSPACE>/dsh-nexus-phases\reviews\integration-tauri-test.log` |

The first pnpm install invocation was issued from the repository root and exited 1 because that directory has no `package.json`. This was a verification-command working-directory error; no code changed. The same required offline command then passed from the frontend package directory, and the table/log record the corrected acceptance run.

Generated `node_modules`, Rust targets, and Tauri resource executables are ignored worktree-only artifacts and are not committed.

## Not done

- No main-branch merge, deployment, system runtime installation, package/runtime download, system `PATH` or user-configuration change.
- No native GUI interaction or screenshot acceptance, real Harness start, or real cold-install build-to-Node-launch acceptance.
- No claim that controlled tests cover Unix runtime branches, Windows junction/real mapped-drive behavior, or the future OS-assigned Agent port/identity flow.
- This phase prepares a fixed reviewable integration branch only. Controller review owns the final immutable-HEAD decision; no kernel approval signature is created here.
