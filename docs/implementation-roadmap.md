# 实施状态与后续工作

[English](implementation-roadmap.en.md)


## 已有能力

Electron 桌面入口、浏览器入口、内置运行时、Harness 安装与关联、版本槽位、认证启动检查、配置修复引导、恢复检查点、内置插件与通知、GitHub 更新及便携包均已有实现。具体边界见[架构](architecture-baseline.md)和[已知限制](known-limitations.md)。这份列表不替代各版本真机验收。

## v0.1.8 已发布

官方 Harness Desktop、统一工作台与托盘维护、依据证据的启动修复、共享离线运行时、便携路径／保存修复，以及 Linux ARM64 AppImage／DEB／RPM 已随版本交付。五个平台目标通过 CI 与安装包检查，中英文指南及截图已同步。用户可见变化见[变更记录](../CHANGELOG.md)，仍需按场景执行的真机验证见[验收清单](acceptance.md)。

## 后续决策

继续按具体用户需求扩展内置插件与公共接口。直接复用上游官方 Desktop，仅支持 Harness 与 Electron 的交集，不继续维护自制客户端。插件市场采用用户选择，不自建包源；上游数据格式跨版本兼容不能由 Launcher 的版本切换机制保证。

[原始实施清单](history/baselines/implementation-roadmap.md)仅保留历史状态，不是当前完成度报告。
