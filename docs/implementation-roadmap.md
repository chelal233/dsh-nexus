# 实施状态与后续工作

[English](implementation-roadmap.en.md)


## 已有能力

Electron 桌面入口、浏览器入口、内置运行时、Harness 安装与关联、版本槽位、认证启动检查、配置修复引导、恢复检查点、内置插件与通知、GitHub 更新及便携包均已有实现。具体边界见[架构](architecture-baseline.md)和[已知限制](known-limitations.md)。这份列表不替代各版本真机验收。

## 当前工作区，尚未随发行包提供

- 便携目录移动后的内置运行时重新定位。
- 运行时设置保存后，下次启动直接采用；当前 Harness 不受影响。
- 避免旧的并发刷新结果覆盖刚保存的设置；持久化 Agent 日志级别。
- 中英文文档与实际界面截图。

以上改动须经[验收清单](acceptance.md)验证，再在变更记录中标注实际发布版本。

## 后续决策

继续按具体用户需求扩展内置插件与公共接口。独立窗口是可选入口，不承诺复制上游 Desktop 的所有功能。插件市场采用用户选择，不自建包源；上游数据格式跨版本兼容不能由 Launcher 的版本切换机制保证。

[原始实施清单](history/baselines/implementation-roadmap.md)仅保留历史状态，不是当前完成度报告。
