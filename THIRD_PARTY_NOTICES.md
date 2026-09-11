# 第三方组件与许可材料

Nexus 的 MIT 许可证仅适用于 Nexus 自有代码，不替代依赖组件的许可证。此页是已确认材料和待完成工作的清单，**不是整个安装包已经完成许可核对的声明**。

| 范围 | 当前材料与核对位置 |
| --- | --- |
| 内置 Node | 完整发行目录保留 `runtime/node/LICENSE` |
| 内置 npm | 完整包保留 `runtime/node/node_modules/npm/LICENSE` 及包内材料 |
| 内置 pnpm | 完整包保留 `runtime/pnpm/LICENSE`；仍需核对打包在其中的第三方组件声明 |
| Rust 依赖 | 根 `Cargo.lock` 与独立 `apps/nexus-launcher/src-tauri/Cargo.lock` 都需纳入精确版本清单 |
| 静态 libgit2 | `libgit2-sys` 包装层许可证不能替代 vendored `libgit2/COPYING`；需保留该原文、链接例外及对应组件声明 |
| 前端依赖与图标 | 根据 `apps/nexus-launcher/pnpm-lock.yaml` 和实际生产构建生成精确版本、版权与许可材料 |

公开分发安装包前，应把核对后的许可原文和组件清单加入包内资源清单，使现有哈希校验覆盖这些文件。不能仅凭包管理器的许可证名称或 Node 的 LICENSE 宣称全包覆盖；缺失材料应逐项记录并补齐。构建身份、资源哈希和代码签名也不能替代此项。
