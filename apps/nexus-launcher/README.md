# Nexus Launcher desktop

Electron is the only desktop host. React renders the control plane; a sandboxed
preload exposes an allowlisted bridge. The Rust stdio adapter reuses
`nexus-launcher-core` and the authenticated Agent API. Business state remains in
Agent; closing the Launcher does not stop services or the independent DSH Shell.

## Develop

```sh
pnpm install --frozen-lockfile
node node_modules/electron/install.js
pnpm prepare:agent
pnpm prepare:runtime
pnpm prepare:notices
pnpm prepare:release
pnpm dev
```

`pnpm dev:web` starts a browser-only Vite preview. `pnpm build` compiles the React
bundle; `pnpm electron:build` packages the desktop application. Resource staging,
icons and common release scripts are under `desktop/`. Rust uses the root
workspace and its single Cargo.lock.

## Verify

```sh
pnpm typecheck
pnpm format:check
pnpm test
pnpm test:electron
node --test tests/*.test.mjs
node scripts/electron-smoke.mjs
```

The smoke uses temporary Nexus/DSH/Harness paths. Set NEXUS_SMOKE_EXECUTABLE to
an unpacked or installed Electron executable to validate that package.
Production builds require signing; `NEXUS_UNSIGNED_SMOKE=1` is for local/CI
acceptance artifacts only. Windows x64/ARM64 and macOS x64/ARM64 are supported
build targets. See ../../docs/github-release.md for publication gates.
