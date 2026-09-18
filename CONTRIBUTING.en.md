# Contributing

[简体中文](CONTRIBUTING.md)


Read the [architecture](docs/architecture-baseline.en.md) and [development setup](apps/nexus-launcher/README.en.md). Scope changes to a reproducible problem or explicit feature, preserving user data, external directories, and active operations.

## Development rules

- Use an isolated branch or worktree and preserve unrelated changes.
- Consume Agent state in the UI; do not duplicate persistent business rules there.
- Shared frontend modules must not depend on pages, and pages must not import `App.tsx`.
- Preserve locking, persistence, process identity, and recovery ordering. Shorter code is not proof of equivalent behavior.
- Keep each built-in plugin in `plugins/<name>`, separate from Launcher and Agent business code.
- Use temporary data roots for tests, never real `.dsh`, sessions, or user profiles.

## Validation

For frontend changes run `pnpm typecheck`, `pnpm format:check`, and relevant tests. Use `pnpm test:electron` for Electron behavior and relevant crate tests for Rust changes. Follow [release gates](docs/github-release.en.md) before publication. Documentation-only work needs link, screenshot, and command checks.

Behavior-preserving refactors should compare outputs, errors, and side effects using the same inputs before and after. Existing frontend commands are `pnpm test:ab capture <directory>` and `pnpm test:ab compare <directory>`; their coverage is limited to declared scenarios. Passing both unit suites is not an equivalence test. Snapshot recovery also has `tests/ab-checkpoint-probe.rs` and `scripts/ab-checkpoint.mjs`; follow their source contract with isolated copies.

## Documentation and pull requests

Maintained guides pair Chinese `.md` and English `.en.md` with reciprocal links. Update both with behavior changes, distinguishing released, unreleased source, planned, and historical evidence. Screenshots must show real UI and record provenance; development previews are not installed-app acceptance.

Describe the trigger, resulting behavior, validation, and unverified scope. Do not commit installers, runtime logs, tokens, private diagnostics, or user-specific paths. Preserve historical evidence rather than rewriting it as a new conclusion.
