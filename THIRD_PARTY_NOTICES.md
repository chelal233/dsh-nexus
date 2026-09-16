# 第三方组件与许可材料

Nexus 的 MIT 许可证仅适用于 Nexus 自有代码，不替代依赖组件的许可证。此页是已确认材料和待完成工作的清单，**不是整个安装包已经完成许可核对的声明**。

| 范围 | 当前材料与核对位置 |
| --- | --- |
| 内置 Node | 完整发行目录保留 `runtime/node/LICENSE` |
| 内置 npm | 完整包保留 `runtime/node/node_modules/npm/LICENSE` 及包内材料 |
| 内置 pnpm | 完整包保留 `runtime/pnpm/LICENSE`；仍需核对打包在其中的第三方组件声明 |
| Rust 依赖 | 根 `Cargo.lock` 纳入精确版本清单；Electron/Chromium 许可随桌面包分发 |
| 静态 libgit2 | `libgit2-sys` 包装层许可证不能替代 vendored `libgit2/COPYING`；需保留该原文、链接例外及对应组件声明 |
| 前端依赖与图标 | 根据 `apps/nexus-launcher/pnpm-lock.yaml` 和实际生产构建生成精确版本、版权与许可材料 |

公开分发安装包前，应把核对后的许可原文和组件清单加入包内资源清单，使现有哈希校验覆盖这些文件。不能仅凭包管理器的许可证名称或 Node 的 LICENSE 宣称全包覆盖；缺失材料应逐项记录并补齐。构建身份、资源哈希和代码签名也不能替代此项。

## 构建时生成的材料

`pnpm prepare:notices` 根据两个 Cargo 锁定依赖图及 pnpm 生产依赖生成 `resources/notices/components.json` 和可找到的许可文本，额外收集 vendored libgit2 的顶层 COPYING 等材料。安装包将这些文件纳入资源哈希校验。清单包含构建依赖和非当前目标依赖，属于保守汇总，不等于最终二进制的精确链接清单。

`reviewRequired: true` 表示未发现许可文本或许可证声明，公开前需补齐核对；自动收集不能证明嵌套 vendored 组件、pnpm 打包组件的全部义务已履行。Node/npm/pnpm 自带许可材料仍保留在 runtime 中。不得用相同 SPDX 名称的其他组件文本代替缺失的版权声明。
