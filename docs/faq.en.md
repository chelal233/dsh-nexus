# Frequently asked questions

[简体中文](faq.md)


## How does Nexus differ from Harness?

Nexus manages local execution, versions, and recovery. Harness provides conversations, Agents, models, and business plugins. Updating Nexus does not upgrade Harness.

## Must I use official Desktop or a plugin market?

No. Web uses your system browser; Desktop launches the official client included in a managed Harness release. You may choose no plugin market. Desktop appears only when supported by the selected Harness and platform; Nexus no longer maintains a replacement client.

## Can I copy only the portable EXE?

No. Keep the entire extracted directory, including `resources/app.asar`, update configuration, and runtimes. Installation-free does not mean data moves with the program. See [troubleshooting](troubleshooting.en.md) for the v0.1.7 relocation issue.

## Must I restart Nexus after saving settings?

Normally no: the next Harness start should read new settings while the running instance remains unchanged. v0.1.8 fixes stale launch paths overriding runtime saves; v0.1.7 may still be affected. Explicit next-process-start settings, such as Agent log level, are separate.

## Are missing sessions after a downgrade deleted?

Not necessarily. Check `DSH_HOME` and profile, then upstream data-format differences. Preserve backups and try the version that could read the data before resetting anything. Nexus cannot guarantee upstream data downgrade compatibility.

## Do passing checks guarantee plugin compatibility?

No. Basic checks, manifest declarations, actual startup, and task acceptance are separate evidence levels.

## Does Launcher focus suppress completion notifications?

Unfocused-only task notifications refer to the actual task page, not Launcher focus. Check event categories, the observer plugin, OS permissions, and do-not-disturb.

## Can it run offline?

After acquiring a supported Harness release, Nexus supplies runtime dependencies and prepares Desktop without online supplementation. Full offline packages can be imported and started without a network on the same OS and architecture; configuration/data-only packages are insufficient. Remote models and network plugins still need connectivity.

## Why retain old version documents?

They preserve decisions and acceptance evidence. Use the [documentation index](README.en.md) for current guides. Historical Tauri/x86 details, test counts, and machine paths are not current release promises.
