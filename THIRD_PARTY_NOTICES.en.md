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

From the Launcher directory, `pnpm prepare:notices` collects both Cargo dependency graphs, pnpm production dependencies, available license texts and vendored libgit2 notices into `resources/notices/components.json` and associated files. Resource hashes cover these materials. The inventory conservatively includes build and non-target dependencies; it is not an exact binary link map.

`reviewRequired: true` identifies missing license text or declarations for manual review before public distribution. Automated collection cannot establish complete compliance for nested vendored or pnpm-bundled components. Keep Node/npm/pnpm's original notices. Never substitute another component's copyright text merely because its SPDX identifier matches. Build identity, hashes and code signing do not replace license review.
