Nexus Launcher 开发预发布，尚未声明稳定版支持。

附件按 Rust target 标明操作系统和架构：Windows x86、x64、ARM64；macOS Intel x64、Apple Silicon ARM64。Windows x86 内置 Node 22，其余内置 Node 24。安装要求见仓库 README 和 docs/github-release.md。

各目标附 SHA256SUMS 与 build.json，可核对提交、版本、构建编号及校验值。CI 执行编译、自动化测试和资源校验；这些不代表真实机器安装、升级、卸载与 Harness 工作流验收。

维护者公开草稿前填写：

- 本版变化：待填写。
- 各架构实际机器验收及已知限制：待填写。
- 第三方许可材料核对结果：待填写。
- Windows 安装包未签名；macOS 仅 ad-hoc 签名，未 Apple 公证。签名发行暂未配置。

不要把完整 CI 日志、诊断目录或私有配置作为 Release 附件。
