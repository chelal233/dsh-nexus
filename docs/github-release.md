# GitHub 发布准备

当前仓库已开始准备公开资源，但尚未推送、创建 Release 或运行 GitHub 远端 CI。不要把此文档当作发布许可核对、历史秘密扫描或真实机器验收已经完成的证据。

## 已有基础

- 用户 README、开发说明、贡献约定、安全问题报告方式与 Issue 模板。
- 沿用 Cargo 声明的 MIT 项目许可证正文；[第三方许可清单](../THIRD_PARTY_NOTICES.md) 另行跟进。
- Windows CI 配置使用只读仓库权限，不自动发布；首次远端执行结果仍待确认。
- 本地 `pnpm release:gate` 关联构建编号、源码摘要、自动化结果和安装包哈希；真实验收保持独立状态。

## 公开前仍需完成

1. 核对计划公开的文件与其 Git 历史，尤其是既有 `artifacts/takeover` 材料、本机路径和历史诊断内容。新增 `.gitignore` 不会清除已跟踪文件或历史。本地工具还可能创建非发布用途的内部引用，不使用 `git push --mirror` 或未经核对的全分支推送。
2. 生成并核对精确版本的第三方清单和许可原文，加入安装包资源校验。此工作尚未完成，不能用当前简表代替。
3. 从审核后的明确提交构建，确认标签、提交、源码摘要、版本、构建编号和安装包对应；当前开发候选包不等于正式标签产物。
4. 完成 EXE/MSI 包内提取校验以及真实机器验收。当前没有验收环境时继续标记待执行；若开放试用，应明确标记预发布及未验证范围。
5. 在实际仓库确认 CI、分支规则、维护者及私密漏洞报告入口。不假定配置文件存在即代表远端功能已启用。

## Release 附件范围

公开附件仅选择本次构建的安装包、SHA-256 校验值和经过检查的简短验证摘要。摘要包含版本、构建编号、提交、检查名称及结果、真实验收状态和已知限制即可。

不要上传完整 `verify-*` 目录、`verification.json` 的原始测试输出、诊断目录或私有恢复备份：它们可能带有宿主绝对路径和日志。内部保留完整证据，公开摘要不包含凭据、用户配置或命令输出原文。

EXE/MSI 的不同语言产物使用同一构建编号；失败重建产生新编号，不用同名旧包覆盖新结果。签名、自更新继续延期；多用户服务、完全离线首次安装及独立插件依赖管理不因此扩展。

参考：[GitHub Releases](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases)、[仓库许可证](https://docs.github.com/zh/repositories/managing-your-repositorys-settings-and-features/customizing-your-repository/licensing-a-repository)。
