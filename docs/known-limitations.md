# 问题修复与验收记录

更新：2026-09-11。R4 终审后的 11 个记录项已按用户新授权处理：10 项修复，1 项因首发版本没有历史兼容要求而不适用。此次允许调整未发布的内部格式与接口；正式发布后再维护版本兼容。

KL 与 KN 批次均已通过外部验收。KL-12 的 REBUTTED 举证已获 reviewer 亲自核实接受，以误报关闭，不需要代码改动。KN 六项全部 PASS；新增 4 条低危／信息备注仅归档，本轮不修复。全部审查批次已关闭，无未决分歧；真机验收和提交安排仍待后续执行。

## 上一轮外部验收

R1～R4 外部 Review 循环已关闭，无未决分歧。R4 的 21 个文件及全部差异经独立复核，哈希一致、无夹带；R4-4 为接受附注的通过，其余 R4 项目通过。

终审独立复跑结果：Rust 19 套件合计 529 通过、0 失败，`cargo_exit=0`；前端 112/112；TypeScript 检查通过。Rust 统计包含 4 个子进程测试，另有 7 个既有 ignored。以上是自动化与代码审查结果，虚拟机/真机验收仍由用户执行。

本地详细证据位于 `target-rtest/review-r4-20260911/`：`R4-fixes.md`、`before/`、`R4.diff`、`changed-files.json` 及测试日志。该目录为本地验证产物，不保证随源码仓库分发。

## KL 批次自动化与外部验收

- `cargo test --workspace --locked -j2 -- --test-threads=4`：19 个套件摘要，合计 533 通过、0 失败（含 4 个子进程测试，7 个既有 ignored），退出码 0。
- 前端 `npm.cmd test`：113/113；`npm.cmd run typecheck`：通过。
- 离线空间与迁移脚本：9/9，包含真实 YAML、`slot: null`、8 种组件选择组合和凭据保留／替换。
- 发布脚本：5/5；Tauri 原生测试：10/10。
- KL 外部 reviewer 已完成 `before/` + `KL.diff` 逐 hunk 复审，21/21 文件哈希双向一致，独立复跑 Rust 533/0、前端 113/113、TypeScript 通过；KL-07/08 为 PASS-WITH-NOTES，其余适用项 PASS。此结论不等同于用户真机验收。

## 终审备注的处置

以下记录保留原编号，便于与外部 Review 对账。

| 编号 | 来源 | 已知行为与影响 | 代码位置 |
| --- | --- | --- | --- |
| KL-01 | R4 终审备注 1 | **已修复**：准备标记直接原子重命名为 manifest，取消发布成功后的删除步骤。重命名失败仍保持未发布状态。 | `crates/nexus-core/src/lib.rs`，`publish_prepared_manifest` |
| KL-02 | R4 终审备注 2 | **已修复**：隔离目录接入维护清理，受保留期限、用户选择和文件指纹约束；删除目录后中断的残留记录也能再次预览清理。 | `crates/nexus-core/src/maintenance.rs` |
| KL-03 | R4 终审备注 3 | **不适用**：用户明确本次为全新首发版本，不支持未发布旧二进制读取新 journal；没有增加兼容层。当前版本事务恢复仍有回归保障。 | `crates/nexus-core/src/config_transaction.rs` |
| KL-04 | R4 终审备注 4 | **已修复**：历史查询只读，只返回最近 10 条；裁剪在归档时进行，并容忍并发删除已过期文件。 | `crates/nexus-agent/src/canary.rs` |
| KL-05 | R4 终审备注 5 | **已修复**：只有 zh、CN、SG、Hans 使用简体；zh-yue 等其他子标签回退英文。 | `apps/nexus-launcher/src/i18n.ts` |
| KL-06 | R4 终审备注 6 | **已修复**：容量错误提供中文原因和可执行操作，同时保留后端原文以便诊断。 | `apps/nexus-launcher/src/App.tsx`、`i18n.ts` |

## 此前记录项的处置

| 编号 | 来源 | 已知行为与影响 |
| --- | --- | --- |
| KL-07 | R3-7 | **已修复**：配置合并后统一迁移路径字段，包括保留凭据中的文件路径；支持 Windows 大小写、命名空间和 UNC。密钥值、会话文本、URL 与相邻目录名保持原值。真实 YAML 由 Agent 内置解析器处理，不依赖 Harness 槽位。 |
| KL-08 | R3-7 | **已修复**：普通 token/password 控制项按精确名称与数值/布尔类型区分；未知敏感字段继续保守过滤。导出按待导出值判断；导入类型变化的验收附注也已在 KN 批次处理，见 KL-N02。 |
| KL-09 | R3-7、R4-13（合并） | **已修复**：失败编号可以重新验证，每次尝试和已有报告原始字节保留；并发验证和成功编号重用被拦截。 |
| KL-10 | R3-7 | **已修复**：凭据恢复记录接入维护清理；保护当前操作引用、最近三份、保留期内和无法识别的记录，只删除所选记录文件，不删除其指向的旧 home 或凭据。 |
| KL-11 | R4-13 | **已修复**：自动切换使用独立 attempt ID；同值显式保存、ABA 及完整配置替换撤销自动回滚权限。普通偏好保存保留该权限，undo 也按 ID 核对归属。 |

## KL-12：未知顶层配置字段校验

**对账结论：REBUTTED（属性未移除）**。当前 `crates/nexus-core/src/lib.rs:6375` 仍有 `#[serde(deny_unknown_fields)]`，作用于 `NexusConfigFile`。KL 的修改前副本在第 6360 行也有该属性；`KL.diff:1272` 将它列为未变更的上下文。新增的 `update_attempt_id` 是显式声明字段，没有放宽其他顶层字段。

**外部验收状态：已关闭**。Reviewer 已亲自复核属性、差异上下文及哈希，确认原复审代理误报，接受 REBUTTED；此项没有代码修复需求。

本次核对的 `lib.rs` SHA-256 为 `11fcf857d539c4f94542d3d9d8835db758e0a1980c226ec66490090cb7b9db38`，与已验收 `target-rtest/review-kl-20260911/changed-files.json` 的 `after_sha256` 一致。没有恢复属性或接受静默忽略字段的改动。

既有回归 `config_transaction::tests::revision_cas_rejects_stale_raw_bytes_and_preserves_unknown_formats` 覆盖未知顶层字段、未知嵌套字段及非法 schema：读取、保存和恢复均须拒绝，当前配置与上一份配置字节保持原样，且不产生待提交事务。KL-12 定向复跑证据单独保存在 `target-rtest/review-kl12-20260911/`，不覆盖已验收 KL 产物。

## KL 验收新增附注的修复（KN 批次）

以下对应外部 reviewer 的 6 条归档项，已按后续授权处理并全部通过外部验收。其中 KL-N02/N03/N06 带低危附注，见下方 KN 验收新增备注。保留问题来源和实现边界供复核。

| 编号 | 已知行为、影响及处理边界 | 代码位置 |
| --- | --- | --- |
| KL-N01 | **已修复**：路径键识别增加 `dirs`、`directories`、`files`、`paths`、`roots`、`homes`，支持其数组值及 camelCase／下划线／连字符字段。沿用路径边界判断，不改写不透明密钥或相邻目录。 | `crates/nexus-agent/scripts/offline-package.mjs`，`pathKey` |
| KL-N02 | **已修复**：仅当新旧值都符合普通控制项的名称与类型时才接受配置更新；任何一侧被识别为凭据，都保留接收方旧值。覆盖数值／布尔与字符串／对象之间的变化、嵌套对象和具名数组项。 | `crates/nexus-agent/scripts/offline-package.mjs`，`keepCredentials` |
| KL-N03 | **已修复**：核对全部不可变 attempt 报告；没有成功或不确定证据阻止时允许同 ID 重新完整验证。KA 后续将原始 latest 副本的保存移到准入通过后，并按内容去重。不可变报告损坏、未知状态、成功或未完成的记录仍拒绝重用；不会从损坏报告推定验证已通过。 | `apps/nexus-launcher/src-tauri/scripts/release-gate.mjs`，`admitVerification` |
| KL-N04 | **已修复**：KN 的操作系统 IPC 锁已由 KA 改为文件锁。真实输出目录及路径别名、不同 build ID 共用锁；持锁进程退出后锁由内核释放。磁盘 `active.lock` 仅作为诊断，取得互斥后保留陈旧标记并自动退役，无须手工删除。锁释放不证明构建子孙进程已结束，因此原有未完成报告继续阻止同 ID 重试。 | `apps/nexus-launcher/src-tauri/scripts/release-gate.mjs`，`verificationLock` |
| KL-N05 | **已加固并补回归**：原执行阶段已有所有权／指纹保护，本轮将普通目录检查提前至候选准入，即使存在 ownership，替换文件或 reparse 对象仍不可选。只有目标确实缺失且存在合法 ownership 时才允许退役残留记录，保持中断续清理能力。 | `crates/nexus-core/src/maintenance.rs`，隔离对象候选分支 |
| KL-N06 | **已修复**：历史逐项流式读取，内存最多保留 10 条摘要，查询保持只读。正常归档后按时间及 ID 稳定排序裁剪；KA 后续增加 64 条容量门槛，到限后须先完成裁剪才可追加。128 条积压及重复归档失败后恢复已覆盖。 | `crates/nexus-agent/src/canary.rs`，`visit_history` / `archive` |

KN 的修改前副本、差异、文件哈希、测试日志和报告位于 `target-rtest/review-kn-20260911/`，没有覆盖已验收 KL 或 KL-12 证据。此次没有安装、打包、提交或真机操作。

本批验证：Rust 19 个套件摘要，535 通过、0 失败（含 4 个子进程测试，7 个既有 ignored）；前端 113/113，TypeScript 通过；离线脚本 11/11，发布脚本 8/8。发布锁与报告、隔离清理与 Canary 分别完成独立静态复核，未发现本轮增量阻断。真机验收仍由用户执行。

KN 外部验收：Reviewer 基于 `before/` + `KN.diff` 逐 hunk 复审，7/7 文件 SHA-256 双向一致，重算 diff 逐字一致、无夹带；独立复跑 Rust 19 套件 535 通过／0 失败（`cargo_exit=0`）、前端 113/113、TypeScript 通过。KL-N01～N06 全部 PASS。连同此前 P1、R2/R3/R4、KL 各批次，本轮 review 循环全部关闭，无未决分歧。

## KN 验收新增备注的修复（KA 批次）

来源：KN 外部验收反馈。用户随后授权修正以下 4 条，现已完成实现和本地自动化验证；KA 的外部复审尚未执行。

| 编号 | 级别 | 已知行为与影响 | 代码位置 |
| --- | --- | --- | --- |
| KN-A01 | 低 | **已修复**：只有通过全部准入检查的尝试才保留旧报告，副本按 SHA-256 内容去重并核对原始字节。重复拒绝不再向已有构建目录增加副本。拒绝本身仍保存独立失败报告。 | `apps/nexus-launcher/src-tauri/scripts/release-gate.mjs`，`admitVerification` |
| KN-A02 | 低 | **已修复**：历史达到 64 条时先清理旧记录，清理持续失败便拒绝新增历史文件；正常保存的最新操作结果仍可查询，恢复删除权限后历史收敛至最近 10 条。已有超额积压先裁剪，不继续增加。 | `crates/nexus-agent/src/canary.rs`，`archive_with_cleanup` |
| KN-A03 | 信息 | **已修复**：实际包内值因凭据保护未应用时，成功导入结果提供中英文说明及冲突选项指引。只传递计数和固定提示，不输出字段名或凭据值；缺省字段、相同值、替换策略不误报。 | `crates/nexus-agent/scripts/offline-package.mjs`、`src/offline.rs`；`apps/nexus-launcher/src/App.tsx`、`i18n.ts` |
| KN-A04 | 信息 | **已修复**：使用 Node 24 内置 SQLite 的排他文件锁，移除可预测的 IPC 名称；保留别名及跨 ID 互斥、进程退出释放和 running 证据保护。Windows 继承构建目录 ACL；Unix guard 仅限所有者。此处不提供针对能修改构建目录或控制同用户进程的对抗安全隔离。 | `apps/nexus-launcher/src-tauri/scripts/release-gate.mjs`，`verificationLock` |

KA 的修改前副本、增量差异、文件哈希、测试日志与 `KA-fixes.md` 保存在 `target-rtest/review-ka-20260911/`，未覆盖已验收 KN 产物。具体测试结果见该报告。新增发布锁在 Windows / Node 24 上实测，未声称完成 Linux 或虚拟机验收。

## 已接受的实现边界及剩余交接

- 托盘状态采用 180 秒缓存窗，属于有界陈旧状态；超时后禁用相关操作。此次未改为 Rust 自主维护实时状态，终审已接受此范围。
- 虚拟机/真机验收由用户按明确构建编号执行，见 [手动验收清单](manual-acceptance-0.1.2.md)。
- 工作区修复尚未提交；提交及后续打包、发布节奏另行确定。外部 Review 通过不代表这些交付动作已执行。
- 凭据迁移按明确的字段语义处理，不猜测任意插件中的不透明字符串；包含账号凭据仍由用户选择。CAS 身份约束覆盖 Nexus 的配置发布路径，不承诺识别外部程序逐字节复制同一文档的操作意图。
- KL 与 KN 的差异、修改前副本及测试证据分别位于 `target-rtest/review-kl-20260911/` 和 `target-rtest/review-kn-20260911/`，与 R4 验收产物分开保存。
