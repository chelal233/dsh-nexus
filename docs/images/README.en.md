# UI screenshots

[简体中文](README.md) · [Back to the overview](../../README.md)

## Capture record

All six images were captured from the real Nexus window on **2026-10-05 (UTC)**. The application ran as an ordinary non-root user from unchanged files extracted from the official v1.0.5 Linux x64 DEB. It was not installed through the system package manager, and these are not development previews or redrawn interfaces.

| Item | Actual record |
| --- | --- |
| Capture date and timezone | 2026-10-05, UTC |
| Nexus version and full commit | v1.0.5 / `e72b324934b174f1702b5111af87cb460f661432` |
| Build identity | `electron-37218933001-1-x86_64-unknown-linux-gnu` |
| Operating system and architecture | Debian GNU/Linux 13, x86_64; Intel Xeon Platinum 8573C |
| Application source | Official Linux x64 DEB, extracted unchanged and run without system installation |
| Window and image dimensions | 1180 × 812 pixels; no cropping or resizing |
| Zoom and theme | 100% page zoom; system theme, rendered light |
| Runtime state | Nexus Agent online; Harness not installed or running; browser mode; empty isolated `web` profile |
| Data and sensitive-information review | Isolated first-use data, no credentials or real user sessions; all six images inspected |
| Pages and languages | Setup guide, Workbench, and Settings opened through actual navigation; language switched through the UI between English and Simplified Chinese |
| Image processing | Native window PNG captures encoded as same-size JPEGs at quality 94; no UI rewriting, compositing, or generated content |

Sources: [v1.0.5 Linux x64 DEB](https://github.com/chelal233/dsh-nexus/releases/download/v1.0.5/dsh-nexus_1.0.5_linux_x64.deb), [platform build record](https://github.com/chelal233/dsh-nexus/releases/download/v1.0.5/dsh-nexus_1.0.5_linux_x64_build.json), and [platform checksum manifest](https://github.com/chelal233/dsh-nexus/releases/download/v1.0.5/dsh-nexus_1.0.5_linux_x64_SHA256SUMS.txt). The captured DEB's SHA-256 is `b693d2a14fc141a1ce3e841ee6b67ea4459f036da47c59b4f92fce8fe0842159`, matching its release-asset record.

The automatic update check encountered a certificate-trust error in the cloud environment. That error was not bypassed, and Harness was not downloaded. This capture does not establish successful online update download or installation.

## Images

| Page | Chinese | English |
| --- | --- | --- |
| Setup guide | [guide-zh.jpg](guide-zh.jpg) | [guide-en.jpg](guide-en.jpg) |
| Workbench | [workbench-zh.jpg](workbench-zh.jpg) | [workbench-en.jpg](workbench-en.jpg) |
| Settings | [settings-zh.jpg](settings-zh.jpg) | [settings-en.jpg](settings-en.jpg) |

## What the images establish

Screenshots show the official release application's first-use UI and actual state. They do not establish acceptance of Harness startup, full sessions, offline migration, system installation/upgrades, notification delivery, or other platforms.

This capture shows Linux Web mode: the bundled Harness lock has no official Linux Desktop resources, and Harness was not installed in the capture environment. Desktop availability on other platforms depends on the selected release and runtime capabilities, not these screenshots. Settings images show the top of the page; more controls require scrolling.

## Refresh requirements

- Use isolated data, with no real conversations, credentials, private paths, or unreviewed diagnostics.
- Capture the actual application UI, select the language, wait for rendering, and check the page title, navigation, current state, and full frame.
- Cover the same pages in Chinese and English. Record actual theme, dimensions, and zoom; do not invent a running Harness or successful state.
- Record any image format conversion, resizing, or cropping. Do not generate or composite features into the captures.
- Update all six images and both language records. Record the version, commit, source, environment, and actual state, and finish verification before publication.
