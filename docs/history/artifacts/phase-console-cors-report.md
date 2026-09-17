# Console CORS phase report

## Scope

- Add a fixed-origin CORS boundary to the loopback Agent so the dependency-free
  Console can call the v1 API from its documented local server.
- Keep Harness supervision, Nexus state, and the loopback-only bind unchanged.
- Update the Console and architecture documentation with the supported origins.

## Acceptance

- `http://127.0.0.1:3091`, `http://localhost:3091`, and `http://[::1]:3091`
  receive CORS headers for Agent GET/POST responses.
- Browser JSON requests receive a successful `OPTIONS` preflight response.
- Other origins, methods, and request headers are not granted CORS access.
- No wildcard origin, credentialed CORS, remote bind, or Harness source change is
  introduced.

## Verification

- `cargo fmt --all -- --check` — passed.
- `cargo test --workspace --locked` — passed (35 tests).
- Isolated HTTP smoke against both debug and release Agents — passed: allowed
  preflight `204`,
  allowed GET `200` with `Access-Control-Allow-Origin`, rejected origin `403`.
- `cargo build --workspace --release --locked`, Console syntax check, and
  `git diff --check` — passed.
