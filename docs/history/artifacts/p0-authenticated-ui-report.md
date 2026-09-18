> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../README.md) · [当前文档 / Current documentation](../../README.md)

# P0 authenticated Harness UI fallback report

Task: `p0-integration` authenticated UI fallback
Worktree: `E:\git\dsh-nexus-phases\p0-integration`
Base: `fdc7e353a6b54f8753df683e879bb3b454276bd2`

## Delivered

When the validated current Harness session includes an authentication token,
the native Launcher does not load the known-to-fail authenticated page in its
iframe. The Web panel explains that this session needs top-level browser
authentication and exposes the existing `Open in system browser` action, which
continues to use the validated native bridge route. The token is not placed in
the Web panel URL, button payload, or rendered Web panel text.

The existing generation, log-session, invalidation, and busy gates remain in
force. An invalidated token session renders the browser action disabled. A
validated loopback session without a token retains the safe iframe path.
English and Simplified Chinese strings cover the new user-facing explanation.

## Verification

| Check | Result |
| --- | --- |
| Focused SSR fixture | PASS: token mode has no iframe and an enabled browser CTA; invalidated token mode has no iframe and a disabled CTA; tokenless loopback mode retains the iframe |
| `pnpm run typecheck` | PASS |
| `pnpm test` | PASS: 22/22 |
| `pnpm run build` | PASS: Vite production build |
| `git diff --check` | PASS |

The existing bounded Harness acceptance observed `http://127.0.0.1:3080` and
HTTP 200, while the native iframe showed `dsh web authentication required`.
This change provides the supported system-browser path for that authenticated
session; it does not claim to embed or bypass Harness authentication.
