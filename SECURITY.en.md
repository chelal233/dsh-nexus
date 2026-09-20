# Security reporting

[简体中文](SECURITY.md)


Do not post exploit details, credentials or unreviewed diagnostic bundles in public issues. Use **Security → Report a vulnerability** if enabled. Otherwise, open an issue without sensitive details and request a private contact. This document does not imply private reporting is enabled or promise response times.

Include the Nexus version/build, OS and architecture, minimal reproduction, impact and redacted evidence. Test only environments you are authorized to operate.

Loopback is not authorization. Agent identity and access checks are product boundaries. Harness and plugins are not sandboxed by Nexus; run trusted software only. The project is in early development with no long-term support commitment.

## Verify downloads

Release attachments include `SHA256SUMS.txt`, its Sigstore keyless signature `SHA256SUMS.txt.sig`, and certificate `SHA256SUMS.txt.crt`. The signature binds the manifest to the repository's tag-based `release.yml` workflow. This is separate from operating-system code signing and Apple notarization; inspect the actual release metadata.

Download the manifest, signature, certificate and referenced artifacts, then run:

```sh
cosign verify-blob SHA256SUMS.txt \
  --signature SHA256SUMS.txt.sig \
  --certificate SHA256SUMS.txt.crt \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github.com/chelal233/dsh-nexus/\.github/workflows/release\.yml@refs/tags/'
sha256sum -c SHA256SUMS.txt
```


Install [cosign](https://github.com/sigstore/cosign); verification may query the Rekor transparency log. Per-architecture checksum manifests permit checking a selected package without downloading every platform. Checksums, provenance and operating-system trust are different checks.

macOS ad-hoc signing is not Developer ID signing or Apple notarization. Refer to the individual artifact record; a verified checksum manifest does not grant operating-system trust.
