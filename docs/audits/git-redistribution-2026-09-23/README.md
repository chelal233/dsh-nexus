# 内置 Git 分发材料核查 / Bundled Git distribution materials

## 当前结论 / Current result

2026-09-23：213 份通知和 62 项源码材料已逐项通过 SHA-256 核验。用户选择不捆绑可选 GCM。放行仅针对排除 GCM、保留原始通知、加入补充通知且随发行提供源码包的 Nexus 处理后版本；不表示原始 dugite 归档整体已放行。最终安装包、原生运行和公开附件仍须分别验收。

All 213 notices and 62 source materials passed SHA-256 verification. The user chose to omit optional GCM. Clearance covers only the Nexus-processed distribution with GCM excluded, original and supplemental notices retained, and the source companion supplied with the release. It does not clear the original dugite archive as a whole. Final packages, native execution and public assets remain separate acceptance layers.

## 处理范围 / Processing scope

Windows x64/ARM64 排除 51 个与官方 GCM 包匹配的程序/依赖文件，只删除内置 Git 配置中的 helper = manager；macOS 两架构排除 240 个匹配文件并保留 NOTICE；Linux ARM64 原本没有 GCM。保留 Git、SSH、LFS、证书及原始许可，不修改用户或系统 Git 配置。

Windows excludes 51 matched GCM files and only the bundled manager helper setting; macOS excludes 240 matched files while retaining NOTICE; Linux ARM64 contains no GCM. Git, SSH, LFS, certificates and original licenses remain. User/system Git configuration is unchanged.

## 源码与通知 / Source and notices

source-materials.json 固定 62 项材料的地址、大小与摘要，包含对应 Git/dugite、MSYS/MinGW 源码、补丁、构建脚本、LFS/MPL 依赖和 CA 源码/转换脚本。Windows 外层源码包虽名为 2.52.0.1，内部确含 2.53.0.windows.4 和匹配 PKGBUILD，版本疑点已排除。materials/manifest.json 记录 213 份实际随包通知；complete 仅表示通知采集层完成。

The 62 pinned materials include corresponding Git/dugite and MSYS/MinGW source, patches, build scripts, LFS/MPL dependencies, and CA source/conversion scripts. The Windows source archive named 2.52.0.1 contains the matching 2.53.0.windows.4 source and PKGBUILD. The notice manifest records 213 shipped texts; complete applies only to notice collection.

每个发行页同时提供 dsh-nexus_<version>_git-sources.tar、build.json 与 SHA256SUMS；发布流程要求同提交来源清单、源码 artifact 和实际摘要吻合。离线转发二进制时，应保留源码附件或同等可获取的配套下载。这不是启动时下载依赖，也不采用三年书面源码承诺。

Each release supplies the corresponding git-sources.tar, build.json and SHA256SUMS. Publication requires same-commit provenance, source artifact and matching archive digest. Keep the companion or equivalent accompanying access when redistributing binaries offline. This is not a runtime download and does not rely on a three-year written offer.

## 验收证据 / Acceptance evidence

Windows x64 实际 Git/LFS/SSH 程序及公开 HTTPS 访问通过。macOS 两架构与 Windows ARM64 已验证原始归档摘要、GCM 排除及 Git/LFS 保留，不替代原生运行测试。最终五平台 CI 和发行附件尚待完成。

Windows x64 Git/LFS/SSH executables and public HTTPS access passed. macOS and Windows ARM64 archive checks confirm exact input hashes, GCM exclusion and retained Git/LFS, not native execution. Final five-platform CI and release assets are still pending.

- archive-inventory.json: pinned original archives and conditional processed-distribution clearance.
- materials/gcm-official-file-boundary.json: file-level matching against official GCM packages.
- materials/source-notice-extraction.json: original source notice extraction evidence.
- source-materials.json and materials/manifest.json: pinned source and notice delivery inventories.
- Local evidence: .tmp-ui-audit/gcm-exclusion-platform-results.json and git-without-gcm-runtime.log.

## 一手依据 / Primary sources

- [GPLv2](https://www.gnu.org/licenses/old-licenses/gpl-2.0.en.html)
- [Pinned dugite release](https://github.com/desktop/dugite-native/releases/tag/v2.53.0-4)
- [Build scripts](https://github.com/desktop/dugite-native/tree/v2.53.0-4/script)
- [Git for Windows source](https://github.com/git-for-windows/git/releases/tag/v2.53.0.windows.4)

可选 GCM 的微软依赖存在未确认的再分发授权，因此按用户选择不捆绑该助手；本记录不对上游作侵权认定。

An optional Microsoft dependency of GCM had unresolved redistribution authorization, so the helper is omitted as requested. This record makes no infringement finding against upstream.
