# Runtime status UI report

Task: `nexus-takeover`
Phase: `runtime-status-ui`
Owner: `verify_checkout`
Base ref: `df3cff00cb5e740f223897756f2c7f5bf9c44207`
Branch: `codex/nexus-takeover/runtime-status-ui`
Worktree: `E:\git\dsh-nexus-phases\runtime-status-ui`

## Worktree handshake

`worktree_verified`: root `E:/git/dsh-nexus-phases/runtime-status-ui`, branch
`codex/nexus-takeover/runtime-status-ui`, and `HEAD` equal to the declared
base `df3cff00cb5e740f223897756f2c7f5bf9c44207`. The worktree was clean
before implementation.

## Scope and implementation

The exact write scope is:

- `apps/nexus-launcher/src/App.tsx`
- `apps/nexus-launcher/src/i18n.ts`
- `apps/nexus-launcher/tests/runtime-status.test.ts`
- `artifacts/takeover/runtime-status-ui-report.md`

Settings now contains an independently exported `RuntimeStatusPanel` and local
runtime status state. Only its explicit Check/Refresh action requests
`GET /v1/runtime` through the existing `proxyRequest`; it adds no poller,
download/install action, or new unavailable-tool button. Agent-unavailable,
idle, loading, success, and retryable error states are rendered with existing
Panel, ActionButton, StatusPill, and state-card styles. Failed requests clear
the prior success payload. The display preserves each tool's own version and
does not infer a pnpm version from Corepack. `not_found` and
`corepack_shim_unverified` have stable explanations; other reasons use the
generic unable-to-verify message. Both English and Simplified Chinese copy is
present.

The API endpoint is implemented by a separate Rust phase. This phase does not
claim Rust integration or end-to-end runtime acceptance.

## Test-first and verification evidence

- Initial focused test: `pnpm.cmd test -- --test-name-pattern "RuntimeStatusPanel"`
  failed as expected because `RuntimeStatusPanel` was not exported (8 prior
  tests passed; 3 new tests failed).
- `COREPACK_ENABLE_NETWORK=0 pnpm.cmd install --offline --frozen-lockfile --ignore-scripts` — exit `0`; cache reuse only.
- `COREPACK_ENABLE_NETWORK=0 pnpm.cmd typecheck` — exit `0`.
- `COREPACK_ENABLE_NETWORK=0 pnpm.cmd test` — exit `0`; 11 passed, 0 failed.
- `git diff --check` — passed before commit.

The SSR tests use Vite `ssrLoadModule` and `renderToStaticMarkup` to verify
available tool metadata, stable unavailable reasons, HTML escaping of a path
with special characters, loading state, Agent-unavailable disabling, retryable
errors, stale-success removal, and no request triggered during render.

## Not done

No Rust changes, endpoint implementation, live Agent request, real Harness
launch, system runtime installation, or build was performed. The target
`/v1/runtime` response still requires the separate Rust phase and integration
acceptance.
