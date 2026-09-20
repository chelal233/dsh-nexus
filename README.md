# Nexus Launcher

[简体中文](README.zh-CN.md)


Keep control of your Harness versions, runtime, and data.

Nexus is a local Harness manager for Windows, macOS, and Linux ARM64. Install and switch Harness versions, choose a profile, start Web Harness or the official Desktop, and recover from startup problems in one place.

Runtime dependencies ship with Nexus. Full offline packages include Harness and the required runtimes, so you can import and start on another machine with the same operating system and architecture without downloading missing components.
[Downloads](https://github.com/chelal233/dsh-nexus/releases) · [User guide](docs/user-guide.en.md) · [FAQ](docs/faq.en.md) · [Documentation](docs/README.en.md)

![Nexus workbench](docs/images/workbench-en.jpg)

*The v0.1.8 workbench in an isolated first-use environment, before installing Harness. Unsupported Desktop options are hidden. See [all screenshots and capture details](docs/images/README.en.md).*

## What you can do

- **Use Web or the official Desktop**: open Web Harness in your system browser, or launch the native Desktop included in a supported managed Harness release. Nexus checks availability before showing the Desktop option; it does not maintain a replacement client.
- **Keep versions and profiles under your control**: prepare an upstream version before switching, keep up to eight managed version slots by default, and switch Web profiles from the workbench. Official Desktop manages its configuration in its own window. You can also connect an existing prebuilt Harness directory.
- **Fix startup problems with a clear next step**: distinguish failed plugins, missing service providers, and plugins still waiting to load. Review suggested repairs, temporarily disable an identified plugin when appropriate, then check and retry. Plugin packages and data are retained, and disabled plugins can be re-enabled.
- **Understand what is running**: the workbench and tray expose launch, open, stop, configuration, and maintenance actions. Choose between closing only Nexus and stopping all services before exiting. Preparation shows its stage and elapsed time, with a cancel action.
- **Move a complete environment offline**: export Harness, matching runtimes, and selected data together. Import a full package on the same OS and architecture without online dependency installation. Configuration-only or data-only exports are partial packages.
- **Choose plugins and notifications**: select a plugin market or use none; configure task notifications by event, delivery method, and window focus. Events depend on Harness and its observer plugin.
- **Work in Chinese or English**: both languages are available in the interface, usage documentation, and release notes.

Startup checks go beyond a running process or reachable page: they inspect client plugins and core services, and distinguish checking, limited functionality, failure, and unverified states. They do not verify every conversation, tool, or runtime business operation. Nexus does not replace Harness's agent, sessions, or models. Updating Nexus and switching Harness are separate operations.

## Download and install

| Platform | Architecture | Downloads | Web Harness | Official Desktop |
| --- | --- | --- | --- | --- |
| Windows | x64 | EXE installer, portable ZIP | Yes | Supported Harness releases |
| Windows | ARM64 | EXE installer, portable ZIP | Yes | Not currently available |
| macOS | Intel x64 | DMG, application ZIP | Yes | Supported Harness releases |
| macOS | Apple Silicon ARM64 | DMG, application ZIP | Yes | Supported Harness releases |
| Linux | ARM64 | AppImage, DEB, RPM | Yes | Not currently available |

All five targets passed CI and package checks for [v0.1.8](https://github.com/chelal233/dsh-nexus/releases/tag/v0.1.8). Support follows the intersection of Harness upstream and Electron; there are no Linux x64 or 32-bit x86 packages in this release. Desktop availability also depends on the selected Harness release.

On Linux ARM64, choose DEB for compatible Debian-family systems, RPM for compatible RPM-family systems, or AppImage where supported. Package format alone does not establish compatibility with every deepin, UOS, Kylin, or other distribution/version; system libraries and desktop environment still matter. CI/package checks are not acceptance tests of every distribution or Harness workflow.

Use the release assets and their `_build.json` records to identify a build. Signing status is specific to each artifact; see [security](SECURITY.en.md).

Extract the entire Windows ZIP, then run `Nexus Launcher.exe`. Do not copy only the executable. Portable means installation-free, not that all user data lives beside the executable. On macOS, copy the complete application to its intended location before opening it.

## Start in three steps

1. Open Nexus and confirm its background Agent is online.
2. Install the Harness version you want, import a full offline package, or connect a prebuilt external directory.
3. For Web, confirm the data directory and profile, resolve blocking startup checks, then start and open it in your browser. When supported, choose official Desktop to open its own window and manage its configuration there.

## Offline use and startup preparation

For the supported managed path, acquire the chosen Harness version while online; Nexus supplies the matching runtime dependencies. Local preparation does not download missing Desktop dependencies. Nexus and official Desktop reuse a compatible Electron runtime, and subsequent starts reuse verified local files to reduce duplicate storage and preparation work.

For transfer without a network, export a **full offline package** from a prepared environment and import it on the same OS and architecture. A partial configuration/data export cannot replace that package. An external source checkout must already be built; Nexus does not compile it or install a development toolchain for you.

Offline startup does not make remote model services, plugin downloads, or other network features available offline. Those still need connectivity unless the chosen service itself runs locally.

## Programs and data are separate

| Content | Behavior |
| --- | --- |
| Nexus program | Installed application or complete extracted directory |
| Nexus data | Configuration, version slots, operation records, diagnostics; separate from program files |
| Harness data, `DSH_HOME` | Configurable path; changing it does not move or delete existing data |
| External Harness | Checked and launched, not automatically updated, compiled, or deleted |
| Project workspace | Managed by Harness; distinct from the program directory and `DSH_HOME` |

## Documentation and contributions

- Usage: [user guide](docs/user-guide.en.md), [configuration](docs/configuration.en.md), [troubleshooting](docs/troubleshooting.en.md).
- Development: [contributing](CONTRIBUTING.en.md), [architecture](docs/architecture-baseline.en.md), [releases](docs/github-release.en.md), [plugins](plugins/README.en.md).
- Status: [changelog](CHANGELOG.en.md), [limitations](docs/known-limitations.en.md), [historical evidence](docs/history/README.en.md).

Nexus is independent and is not an official upstream Harness release. Nexus code uses [MIT](LICENSE); dependencies retain their own licenses. See [third-party notices](THIRD_PARTY_NOTICES.en.md).

## Community

- Thanks to the [LINUX DO](https://linux.do/) community for providing an open and welcoming platform for technical discussion.
