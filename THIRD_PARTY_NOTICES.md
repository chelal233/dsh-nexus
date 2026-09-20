# 第三方组件与许可材料

[English](THIRD_PARTY_NOTICES.en.md)


Nexus 的 MIT 许可证仅适用于 Nexus 自有代码，不替代依赖组件的许可证。此页是已确认材料和待完成工作的清单，**不是整个安装包已经完成许可核对的声明**。

| 范围 | 当前材料与核对位置 |
| --- | --- |
| 内置 Node | 完整发行目录保留 `runtime/node/LICENSE` |
| 内置 npm | 完整包保留 `runtime/node/node_modules/npm/LICENSE` 及包内材料 |
| 内置 pnpm | 完整包保留 `runtime/pnpm/LICENSE`；仍需核对打包在其中的第三方组件声明 |
| Rust 依赖 | 根 `Cargo.lock` 纳入精确版本清单；Electron/Chromium 许可随桌面包分发 |
| 静态 libgit2 | `libgit2-sys` 包装层许可证不能替代 vendored `libgit2/COPYING`；需保留该原文、链接例外及对应组件声明 |
| 前端依赖与图标 | 根据 `apps/nexus-launcher/pnpm-lock.yaml` 和实际生产构建生成精确版本、版权与许可材料 |
| 本地插件声明检查 | 内嵌 node-semver 7.7.4（ISC）；原文在 `crates/nexus-agent/src/vendor/semver.LICENSE`，发布时复制至 notices |

公开分发安装包前，应把核对后的许可原文和组件清单加入包内资源清单，使现有哈希校验覆盖这些文件。不能仅凭包管理器的许可证名称或 Node 的 LICENSE 宣称全包覆盖；缺失材料应逐项记录并补齐。构建身份、资源哈希和代码签名也不能替代此项。

## 构建时生成的材料

`pnpm prepare:notices` 根据根 Cargo 工作区锁定依赖图及 pnpm 生产依赖生成 `desktop/resources/notices/components.json`（打包后为 `resources/notices/components.json`） 和可找到的许可文本，额外收集 vendored libgit2 的顶层 COPYING 等材料。安装包将这些文件纳入资源哈希校验。清单包含构建依赖和非当前目标依赖，属于保守汇总，不等于最终二进制的精确链接清单。

`reviewRequired: true` 表示未发现许可文本或许可证声明，公开前需补齐核对；自动收集不能证明嵌套 vendored 组件、pnpm 打包组件的全部义务已履行。Node/npm/pnpm 自带许可材料仍保留在 runtime 中。不得用相同 SPDX 名称的其他组件文本代替缺失的版权声明。

## v0.1.8 新增运行时与平台范围

| 分发材料 | 清单与许可核对范围 |
| --- | --- |
| 共享 Electron / Chromium | Launcher 使用与官方 Desktop 锁一致的 Electron 44.0.0。保留宿主发行目录中的 Electron LICENSE 和 Chromium 第三方声明；共用一套运行时不免除声明保留要求。 |
| 官方 Desktop 离线运行时 | `apps/nexus-launcher/desktop/desktop-runtime-lock.json` 固定上游提交和锁摘要；生成的 `resources/runtime/desktop/lock.json` 按目标列出 Node、CPython/python-build-standalone 和 Python wheels 及摘要，`primary.tar.gz` 包含预组装运行时与 office-skills 资源。除组件清单外，还需核对归档内原始许可、版权与声明。 |
| Linux 静态 OpenSSL | Linux 启用 git2 的 `vendored-openssl`，Cargo.lock 记录 openssl-src 等依赖。Rust 包装层许可文本不能替代所带 OpenSSL 项目的许可材料。 |
| Harness 与用户安装的插件 | 所选 Harness 及插件保留各自许可证；完整离线导出再次分发这些文件时，应连同 Nexus 材料保留原有声明。 |
| 离线 Electron 宿主备份 | 完整导出可能包含 `host.tar.gz`，macOS 包括完整签名应用。应保留宿主原有第三方材料；哈希或签名不能代替许可核对。 |

Windows x64、macOS x64/ARM64 携带受支持的官方 Desktop 资源；Windows ARM64 与 Linux ARM64 携带不支持标记，不含该资源套件。应按实际目标产物核对，不能假定各平台组件完全相同。

## 自动清单未覆盖的核对

`prepare:notices` 当前扫描根 Cargo 元数据、pnpm 生产依赖、可找到的许可文件、vendored libgit2 顶层声明，以及内嵌 semver 许可；它**不会独立展开并审查**每个 Python wheel、CPython 发行包、office-skills 资源、OpenSSL 嵌套源码声明或 Electron 宿主备份。`reviewRequired: false` 仅描述已扫描条目，不代表整个安装包已完成核对。

增加运行时时，应结合 `lock.json`、实际运行时归档和组件原文逐项检查，记录缺失材料，必要时修正打包。此次文档更新说明实际范围，不代表完成法律审查，也不改变已经发布的 v0.1.8 二进制。
