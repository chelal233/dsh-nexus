# Frequently asked questions

[简体中文](faq.md)


## How does Nexus differ from Harness?

Nexus manages local execution, versions, and recovery. Harness provides conversations, Agents, models, and business plugins. Updating Nexus does not upgrade Harness.

## Must I use the independent window or a plugin market?

No. Browser and independent window entries are optional, and you may choose no market. Desktop-specific plugins require actual supported interfaces; Nexus does not promise all private APIs of other projects.

## Can I copy only the portable EXE?

No. Keep the entire extracted directory, including `resources/app.asar`, update configuration, and runtimes. Installation-free does not mean data moves with the program. See [troubleshooting](troubleshooting.en.md) for the v0.1.7 relocation issue.

## Must I restart Nexus after saving settings?

Normally no: the next Harness start should read new settings while the running instance remains unchanged. Current source fixes stale launch paths overriding runtime saves; published v0.1.7 may still be affected. Explicit next-process-start settings, such as Agent log level, are separate.

## Are missing sessions after a downgrade deleted?

Not necessarily. Check `DSH_HOME` and profile, then upstream data-format differences. Preserve backups and try the version that could read the data before resetting anything. Nexus cannot guarantee upstream data downgrade compatibility.

## Do passing checks guarantee plugin compatibility?

No. Basic checks, manifest declarations, actual startup, and task acceptance are separate evidence levels.

## Does Launcher focus suppress completion notifications?

Unfocused-only task notifications refer to the actual task page, not Launcher focus. Check event categories, the observer plugin, OS permissions, and do-not-disturb.

## Can it run offline?

Installing the desktop package does not require downloading Chromium separately. Initial Harness preparation normally uses the network. Prepared local services can start offline; models and plugins have their own network requirements.

## Why retain old version documents?

They preserve decisions and acceptance evidence. Use the [documentation index](README.en.md) for current guides. Historical Tauri/x86 details, test counts, and machine paths are not current release promises.
