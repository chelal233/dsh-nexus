# Known limitations and current status

[简体中文](known-limitations.md)


This page tracks user-visible boundaries rather than historical reviewer batches. Original records remain in the [archive](history/baselines/known-limitations.md).

- v0.1.7 may retain old bundled paths after portable relocation and override saved runtime settings with an old launch path. Current local source fixes these; unrevised packages remain affected.
- Declaration compatibility does not guarantee plugin behavior or data-format compatibility. Downgrading Harness may hide sessions using newer formats.
- Browsers cannot directly expose every Electron native interface. Full compatibility with third-party private Desktop APIs is not promised.
- Bundled runtimes simplify setup but do not remove upstream native dependencies' compiler requirements.
- Incomplete downloads are not installable updates. Portable packages require more than the EXE. End-to-end updates require build-specific acceptance.
- Windows/macOS x64/ARM64 are build targets, not proof of real-device acceptance for every IME, terminal, or permission combination.
- OS code signing, Apple notarization, and checksum-manifest signatures are separate mechanisms; consult artifact metadata.
- Harness/plugins are not a security sandbox. Snapshots are not complete backups of all user data.

New entries should name affected versions, triggers, workarounds, fix versions, and validation level. Do not mark unverified conclusions resolved.
