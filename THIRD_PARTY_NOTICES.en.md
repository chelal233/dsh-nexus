# Third-party components and notices

[简体中文](THIRD_PARTY_NOTICES.md)


Nexus's MIT license covers Nexus-owned code only. Dependency licenses still apply. This inventory is not a claim that all licensing obligations for every distributed package have been reviewed.

| Component | Evidence |
| --- | --- |
| Node | `runtime/node/LICENSE` in the complete distribution. |
| npm | `runtime/node/node_modules/npm/LICENSE` and package notices. |
| pnpm | `runtime/pnpm/LICENSE`; bundled third-party notices need review too. |
| Rust | Exact versions in `Cargo.lock`. |
| Electron/Chromium | Notices distributed with the desktop package. |
| Static libgit2 | Vendored `libgit2/COPYING`, linking exception and component notices; the `libgit2-sys` wrapper license is insufficient. |
| Frontend and icons | `apps/nexus-launcher/pnpm-lock.yaml` and actual production dependencies. |
| Local plugin declaration checks | Embedded node-semver 7.7.4 (ISC); `crates/nexus-agent/src/vendor/semver.LICENSE`, copied into release notices. |

From the Launcher directory, `pnpm prepare:notices` collects the root Cargo workspace dependency graph, pnpm production dependencies, available license texts and vendored libgit2 notices into `desktop/resources/notices/components.json` (`resources/notices/components.json` in the packaged application) and associated files. Resource hashes cover these materials. The inventory conservatively includes build and non-target dependencies; it is not an exact binary link map.

`reviewRequired: true` identifies missing license text or declarations for manual review before public distribution. Automated collection cannot establish complete compliance for nested vendored or pnpm-bundled components. Keep Node/npm/pnpm's original notices. Never substitute another component's copyright text merely because its SPDX identifier matches. Build identity, hashes and code signing do not replace license review.

## v0.1.8 runtime and platform additions

| Distributed material | Inventory and notice scope |
| --- | --- |
| Shared Electron / Chromium | Launcher uses Electron 44.0.0, matching the official Desktop lock. Preserve Electron's LICENSE and Chromium third-party notices in the host distribution. Sharing one runtime does not remove notice obligations. |
| Official Desktop offline runtime | `apps/nexus-launcher/desktop/desktop-runtime-lock.json` pins the upstream revision and lock digest. The generated `resources/runtime/desktop/lock.json` lists Node, CPython/python-build-standalone and Python wheels by target and digest; `primary.tar.gz` contains preassembled runtime and office-skill resources. Review the archives' original license/copyright/notice materials as well as their component inventory. |
| Linux static OpenSSL | Linux enables `git2`'s `vendored-openssl` feature; Cargo.lock records openssl-src and related crates. The Rust wrapper's license text does not replace the bundled OpenSSL project's notices. |
| Harness and user-installed plugins | The selected Harness release and plugins retain their own licenses. A complete offline export redistributes those files; retain their original notices alongside Nexus materials. |
| Offline Electron host backup | Full exports may include `host.tar.gz`, including a complete signed app on macOS. Retain the host's original third-party notices; a checksum or signature is not a license review. |

Windows x64 and macOS x64/ARM64 include the supported official Desktop kit. Windows ARM64 and Linux ARM64 carry an unsupported marker instead of that kit. Inspect the actual target artifact rather than assuming every platform contains identical components.

## What the generated inventory does not establish

`prepare:notices` currently scans the root Cargo metadata and pnpm production packages, available license files, vendored libgit2 top-level notices, and the embedded semver license. It does **not** independently expand and audit every Python wheel, CPython distribution, office-skill resource, nested OpenSSL source notice, or Electron host backup. `reviewRequired: false` only describes a scanned inventory entry, not the whole installer.

For runtime additions, inspect `lock.json`, the actual runtime archives, and original component notices; record missing materials and correct the packaging if necessary. This document update identifies the scope and does not certify a completed legal audit or alter already published v0.1.8 binaries.

## Full Git command line (in development)

`runtime/git` uses GitHub Desktop's dugite-native v2.53.0-4 portable distribution with pinned per-platform SHA-256 digests. Git, helpers, certificates and original license files are retained. Windows Git's license is in `runtime/git/LICENSE.txt`; component notices remain in their original directories. Git's GPLv2 license is independent of Nexus's MIT license. Upstream build scripts and source references: https://github.com/desktop/dugite-native/tree/v2.53.0-4 .

Corresponding-source availability and nested component redistribution requirements must still be checked per platform before release; the generated inventory marks this component `reviewRequired`. Local Windows tests do not establish installation acceptance on other platforms.

The 2026-09-23 redistribution audit is complete with a **not cleared** result. See the [Git audit](docs/audits/git-redistribution-2026-09-23/README.md) for evidence and acceptance conditions. `reviewRequired` remains set; it is not an enforced CI publication gate.
