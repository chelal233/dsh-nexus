# 贡献指南

[English](CONTRIBUTING.en.md)


先阅读[架构](docs/architecture-baseline.md)和[开发环境](apps/nexus-launcher/README.md)。修改应围绕可复现的问题或明确功能，保留用户数据、外部目录和正在进行的操作。

## 开发约定

- 在独立分支或工作树中修改，不覆盖无关变更。
- 页面只消费 Agent 状态；不要在 UI 中重建持久化业务规则。
- 公共前端模块不能反向依赖页面，页面不能导入 `App.tsx`。
- 保留事务加锁、持久化、进程身份和恢复顺序；代码变短不等于行为等价。
- 每个内置插件独立放在 `plugins/<name>`，不要混入 Launcher 或 Agent 业务文件。
- 测试使用临时数据根目录，不使用真实 `.dsh`、会话或用户 profile。

## 验证

前端修改执行 `pnpm typecheck`、`pnpm format:check` 和对应测试；Electron 行为执行 `pnpm test:electron`；Rust 修改执行相关 crate 测试。发布前执行[发布门禁](docs/github-release.md)。仅文档修改检查链接、截图和命令准确性即可。

保持行为的重构应使用相同输入比较修改前后的输出、错误和副作用。已有前端工具为 `pnpm test:ab capture <目录>` / `pnpm test:ab compare <目录>`，仅覆盖其声明的场景；不能用单测同时通过代替 A/B 等价性。快照恢复另有 `tests/ab-checkpoint-probe.rs` 与 `scripts/ab-checkpoint.mjs`，按其源码约定使用隔离副本。

## 文档与 PR

当前指南采用中文 `.md` 与英文 `.en.md` 配对，互相链接。同一次行为修改同步两种语言，并区分已发布、源码未发布、计划和历史证据。截图必须是真实界面并记录来源，不能把开发预览当成完整安装验收。

PR 说明触发条件、行为变化、验证结果和未验证范围。不得提交安装包、运行日志、令牌、私人诊断或用户专用路径。历史证据不重写成新结论；需要新结论时在当前指南或新验收记录中明确给出。
