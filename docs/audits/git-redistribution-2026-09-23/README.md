# 内置 Git 公开分发许可核查 / Bundled Git redistribution audit

核查日期 / Audited: 2026-09-23

## 结论 / Decision

核查已完成；当前五个平台的公开分发材料均不完整，**不予放行**。这不是 Git 禁止再分发，也不是已认定上游侵权，而是 Nexus 尚不能以现有材料证明自身满足发行条件。保留 `reviewRequired: true`。该字段本身是记录；CI 现已按五个平台固定归档与摘要检查 `publicRedistributionReady`，未全部放行时不上传安装包，发行流程直接阻断。构建测试通过不代表许可通过。

Audit completed; **public redistribution is not cleared for any of the five targets**. This does not mean Git prohibits redistribution or that upstream is infringing. Nexus lacks sufficient evidence for its own distribution. Keep `reviewRequired: true`; the flag itself is informational. CI now checks `publicRedistributionReady` against all five pinned archives and hashes, withholds package uploads until cleared, and blocks publication. Build success is not license clearance.

## 范围与实物证据 / Scope and evidence

检查实际缓存的 dugite-native v2.53.0-4 五个原始 tar.gz，构建标识 4098283；逐包 SHA-256、许可证文件路径、Windows 包版本清单见 [archive-inventory.json](archive-inventory.json)。同时核对 Nexus prepare-runtime.mjs、prepare-notices.mjs 和固定上游构建脚本。

Inspected the five cached original dugite-native v2.53.0-4 tar.gz archives (build 4098283), Nexus staging/notices scripts, and pinned upstream build scripts. The adjacent inventory records hashes, license paths and Windows package versions. This is a source/archive audit, not a legal opinion or runtime acceptance.

| Target | Evidence / 实物结果 | Remaining obligations / 缺口 |
| --- | --- | --- |
| Windows x64 / ARM64 | 原有 LICENSE.txt 与组件许可目录保留；包含 Git LFS、GCM、MSYS/MinGW 组件 / Original licenses retained; includes LFS, GCM and MSYS/MinGW components | 对应源码与构建材料未配齐；完整嵌套组件映射未闭合 / Corresponding source, build materials and complete nested-component mapping missing |
| macOS x64 / ARM64 | 原始包有 `libexec/git-core/NOTICE`，含 GCM/.NET 通知；旧扫描漏了 NOTICE。含 LFS 3.7.1、GCM 2.9.0 / GCM/.NET NOTICE is present; the earlier filename scan missed it | LFS 通知、GCM 自身 LICENSE 及依赖映射待补；已有 NOTICE 保留，对应源码未交付 / LFS notices, GCM own LICENSE and dependency mapping remain incomplete; existing NOTICE is retained; source delivery missing |
| Linux ARM64 | 同样没有上述许可证文件；含 LFS 3.7.1；构建配置没有该架构的 GCM URL / No matching license files; LFS included, no ARM64 GCM URL configured | LFS 及其依赖通知、CA 材料与对应源码核查未完成 / LFS dependency notices, CA materials and source delivery incomplete |

文件名匹配仅用于定位通知，不证明不存在嵌入式声明，也不证明已找到全部许可。Windows package-versions 是候选组件清单，不能把其中每项都等同于实际打包文件。

Filename matching locates notices, not embedded declarations or every obligation. Windows package-versions is a candidate inventory, not proof every listed package is shipped.

## 已确认的许可原则 / Confirmed requirements

- Git 和 dugite-native 构建代码采用 GPLv2。Git COPYING 已由 Nexus 补入；Windows 原始通知保留。LFS/GCM 等独立组件不能仅标成 Git 的 GPL-2.0-only 就视为已覆盖。
- GPLv2 第 3 条要求满足对应源码交付途径之一；线上发行可在二进制同一下载位置提供等效源码访问。源码应包括实际对应版本、补丁及编译/安装控制脚本。仅链接一个 GitHub 首页或 tag、仅提供许可证文本，都不足以证明已履行。
- 未采用三年书面源码承诺，不替维护者凭空作长期履约承诺；也未声称符合仅适用于特定非商业转发的 3(c) 例外。
- Nexus 通过独立可执行程序调用 Git；简单聚合本身不要求把 Nexus MIT 改为 GPL，但这不免除所附 Git 组件各自的分发义务。

Git and dugite-native build code use GPLv2. Nexus adds Git COPYING and retains Windows notices. Separate components require their own notices. GPLv2 section 3 requires a corresponding-source delivery route, including exact source, patches and build/install scripts. A repository link or license text alone does not demonstrate compliance. No three-year written offer or section 3(c) exception is asserted. Separate-process aggregation does not by itself relicense Nexus MIT, but bundled components retain their obligations.

## 具体未决证据 / Specific unresolved evidence

补充核实已解除 Windows Git 版本疑点：官方 `mingw-w64-git-2.52.0.1-1.src.tar.gz` 内实际包含 `mingw-w64-git/git-v2.53.0.windows.4.tar.gz`，其 `mingw-w64-git/PKGBUILD` 写明 `tag=2.53.0.windows.4`、`pkgver=2.52.0.1`。这是包元数据与 Git 标签不同，不能再作为源码不对应的证据。上游已有该源码；Nexus 尚未完成收集、核验及发行配套交付。

Follow-up inspection resolved the apparent Windows Git version mismatch: the official source archive contains the exact 2.53.0.windows.4 source tarball and its PKGBUILD explicitly uses that tag with package version 2.52.0.1. The differently named outer archive is not evidence of missing matching source. Nexus still needs to collect, verify and deliver the source companion and map the actually shipped MSYS/MinGW components.

旧许可证扫描漏掉 `NOTICE`。两个 macOS 包有 `libexec/git-core/NOTICE`，Windows x64/ARM64 分别有 `mingw64/doc/git-credential-manager/NOTICE` 与 `clangarm64/doc/git-credential-manager/NOTICE`。清单已补正。Git LFS 仍缺其 MIT/Go 及实际 Go 模块通知，不能用 Git COPYING 替代。

The earlier scan omitted NOTICE filenames; the inventory now includes the verified macOS and Windows GCM NOTICE files. Git LFS-specific MIT/Go and actual dependency notices remain separate obligations; Git COPYING does not cover them.

## 公开发行放行条件 / Release acceptance

1. 为每个实际随包组件记录版本、来源、许可证、通知路径；补齐 LFS、GCM/.NET 和相关依赖通知，不移除现有通知。
2. 收集并验证与二进制对应的 GPL/LGPL 源码、补丁与构建脚本；Windows 按实际文件映射 MSYS/MinGW 包，不只保存 Git 主仓库。
3. 形成带摘要的源码配套包，随 Nexus 发行提供等效下载；离线再分发应附带所需源码材料或另行满足许可证允许的交付方式。源码配套包无需进入日常运行路径，不改变离线运行原则。
4. 核验最终安装包通知与发布页源码附件，再解除待审。若选择委托外部站点托管，需有符合要求的可用性安排，不能假设第三方链接永久有效。

Inventory every shipped component and restore notices; verify exact GPL/LGPL source, patches and build scripts; publish checksum-verified source companions with equivalent access; account for offline redistribution; inspect final notices and source attachments before clearing review. External hosting requires an adequate availability arrangement rather than an assumed permanent link.

## Primary sources / 一手依据

- [GPLv2 text, sections 2 and 3](https://www.gnu.org/licenses/old-licenses/gpl-2.0.en.html)
- [FSF GPLv2 FAQ](https://www.gnu.org/licenses/old-licenses/gpl-2.0-faq.html)
- [Pinned dugite-native release](https://github.com/desktop/dugite-native/releases/tag/v2.53.0-4)
- [Pinned dependency manifest](https://github.com/desktop/dugite-native/blob/v2.53.0-4/dependencies.json)
- [Pinned build scripts](https://github.com/desktop/dugite-native/tree/v2.53.0-4/script)
- [Git for Windows release](https://github.com/git-for-windows/git/releases/tag/v2.53.0.windows.4)

本次只形成核查结论与证据，未修改运行时或 D 盘安装，未推送 GitHub，未宣布公开发行通过。许可补料尚未完成。后续验收已加入发行阻断自动化；此核查本身不构成放行。

This audit changes evidence/documentation only, not runtime binaries or the D-drive installation. Nothing was pushed or cleared for release. License remediation remains open. Subsequent acceptance work added automated publication gating; this audit does not grant clearance.
