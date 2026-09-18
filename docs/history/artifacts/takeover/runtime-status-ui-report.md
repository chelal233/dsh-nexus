> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../../README.md) · [当前文档 / Current documentation](../../../README.md)

# Runtime status UI fix report

Task: `nexus-takeover-fix1`
Phase: `runtime-ui-fix1`
Owner: `verify_checkout`
Base ref: `809dda1f6b327449e09b4cee5bbbf7dcba5e58a6`
Branch: `codex/nexus-takeover/runtime-ui-fix1`
Worktree: `<WORKSPACE>/dsh-nexus-phases\runtime-ui-fix1`

## Worktree handshake

`worktree_verified`: root `<WORKSPACE>/dsh-nexus-phases/runtime-ui-fix1`, branch
`codex/nexus-takeover/runtime-ui-fix1`, and `HEAD` equal to the declared base
`809dda1f6b327449e09b4cee5bbbf7dcba5e58a6`. The worktree was clean before
the fix.

## Scope and implementation

The exact write scope is:

- `apps/nexus-launcher/src/App.tsx`
- `apps/nexus-launcher/src/i18n.ts`
- `apps/nexus-launcher/tests/runtime-status.test.ts`
- `artifacts/takeover/runtime-status-ui-report.md`

`runtimeStatusFromResponse` now rejects malformed envelopes, non-object,
duplicate, missing, and unknown tool entries. It returns the required
`git`/`node`/`pnpm` order and only accepts an available tool when its version
is non-empty, source is `system` or `nexus`, and path is absolute. Windows
drive-absolute, UNC, and POSIX paths are accepted; relative and drive-relative
paths fail closed.

`createRuntimeStatusController` is a small injectable async controller used by
Settings. It starts in idle, emits loading before the injected transport, calls
only `GET /v1/runtime` after an explicit Check/Refresh action, skips transport
when the Agent is unavailable, and emits success or error with `status: null`.
The controller tests observe the real loading-to-success/error transitions and
verify that a failed refresh clears the previous success payload. SSR tests
remain display-only and verify that rendering does not trigger a request.

Runtime source labels use bilingual i18n: `system` renders as `System` or
`系统`, while `nexus` preserves the `Nexus` brand. The optional provider
initial locale is used only to render the Chinese SSR assertion.

The API endpoint is implemented by a separate Rust phase. This phase does not
claim Rust integration, live Agent requests, or end-to-end runtime acceptance.

## Test-first and verification evidence

- Pre-implementation focused run exited non-zero because the newly referenced
  parser/controller exports were not yet present; the source-label expectation
  was also intentionally ahead of its implementation. This was expected test
  scaffolding, not a behavior RED classification or a pre-existing regression.
- `COREPACK_ENABLE_NETWORK=0 pnpm.cmd install --offline --frozen-lockfile --ignore-scripts` — exit `0`; cache reuse only.
- `COREPACK_ENABLE_NETWORK=0 pnpm.cmd typecheck` — exit `0`.
- `COREPACK_ENABLE_NETWORK=0 pnpm.cmd test` — exit `0`; 14 passed, 0 failed.
- `git diff --check` — passed before commit.

The tests use Vite `ssrLoadModule` and `renderToStaticMarkup` for display and
an injected transport/controller for request, protocol parsing, and state
transition evidence. They cover normal and malformed protocol input, path
forms, HTML escaping, bilingual source rendering, manual-only requests,
Agent-unavailable no-call behavior, loading/success/error, and stale payload
clearing after failed refresh.

## Not done

No Rust changes, endpoint implementation, live Agent request, real Harness
launch, system runtime installation, package download, or build was performed.
