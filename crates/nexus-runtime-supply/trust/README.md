# Compiled runtime verification roots

These files are compiled into `nexus-runtime-supply`; runtime downloads cannot replace or extend them.

## Node release keys

- File: `node-release-keys.asc`
- SHA-256: `5623fcee563025cbd8d95b17bf245a6d85d43279e1612103bbafa5645bc19e4e`
- Source: the public OpenPGP key files in `https://github.com/nodejs/release-keys` at commit `5b7f55f4a7e35d1176d27a6b81b0c3c3b794216b`.
- Construction: 29 ASCII-armored public-key blocks concatenated without modification on 2026-09-05.
- Purpose: verify Node cleartext-signed `SHASUMS256.txt.asc` before trusting the checksum for an exact Node artifact.

The source repository publishes public verification material rather than executable code. No separate license text was copied into this crate. Node's binary distribution remains subject to the license shipped with that release.

## npm registry signing keys

- File: `npm-keys.json`
- SHA-256: `faf23d8753d5bb79df250f10391ac89b63ecf7743e48487a544a99c847f9c8df`
- Source: `https://registry.npmjs.org/-/npm/v1/keys`, retrieved 2026-09-05.
- Content: the two public ECDSA P-256 keys returned by the registry at that time, including key identifiers and expiry metadata.
- Purpose: verify the npm signature over `name@version:integrity` before using the exact pnpm tarball SRI.

The npm response is public registry metadata, not copied program source. The crate uses it only as a fixed verification root.

## Update rule

Changing either file requires a reviewed source-policy revision, updated hashes in this document, and signature fixtures. The runtime must never fetch current keys and treat them as a new trust root in the same request that fetches an artifact. Unknown, expired where applicable, malformed, or non-matching keys fail closed.
