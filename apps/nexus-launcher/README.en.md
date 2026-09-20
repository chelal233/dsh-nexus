# Launcher development

[简体中文](README.md)


Use Rust 1.98.0, Node 24.20.0, and pnpm 11.7.0. Windows needs the MSVC C++ toolchain; macOS needs Xcode Command Line Tools. Workflows and lockfiles define exact versions.

## Run locally

```sh
cd apps/nexus-launcher
pnpm install --frozen-lockfile
node node_modules/electron/install.js
pnpm typecheck
pnpm test
pnpm test:electron
pnpm prepare:agent
pnpm prepare:runtime
pnpm prepare:desktop
pnpm prepare:notices
pnpm prepare:release
pnpm dev
```

Start these commands at the repository root. Resource staging compiles helpers and downloads target-specific runtimes, so the first run may take time. `pnpm dev` opens Electron. `pnpm dev:web` is only a React browser preview and cannot validate native pickers, tray behavior, or automatic updates.

## Build and test

`pnpm build` runs typechecking and Vite. `pnpm electron:build` packages into `electron-dist`. `pnpm release:gate` is the complete local Windows x64 gate, with outputs separate from cross-platform CI. Test resources must match the target architecture.

Production packaging requires signing by default. `NEXUS_UNSIGNED_SMOKE=1` is for explicitly identified development/acceptance builds, not evidence of signing or notarization. See [release engineering](../../docs/github-release.en.md).

Set `NEXUS_DATA_DIR` and `DSH_HOME` to separate test directories. `NEXUS_SMOKE_EXECUTABLE` selects an installed or unpacked Electron executable; passing smoke tests does not prove all business interactions.

Entry points are `src/App.tsx`, `electron/main.mjs`, and `electron/preload.cjs`; build/resource scripts live in `desktop/scripts`. Rust uses the root workspace and Cargo.lock. Put UI test exports in `tests/ui-test-entry.ts`, not the application entry.

## Platform resources

Build Desktop resources on their native platform after `prepare:runtime`, before release manifests. The Electron version in package.json must match `desktop/desktop-runtime-lock.json` (currently 44.0.0). Unsupported Desktop targets produce an explicit marker and retain Web mode; do not download at first user launch. Linux ARM64 helpers use the glibc 2.28 baseline and vendored OpenSSL; follow the native CI toolchain, including Perl/make. This is a build-time requirement, not an end-user toolchain dependency.
