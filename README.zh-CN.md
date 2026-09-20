# Nexus Launcher

[English](README.md)


让 Harness 的版本、运行环境和数据由你掌控。

Nexus 是本地 Harness 管理工具，支持 Windows、macOS 和 Linux ARM64。在一个地方安装与切换 Harness 版本、选择配置档、启动 Web 或官方 Desktop，并处理启动问题。

运行依赖随 Nexus 提供。完整离线包包含 Harness 和所需运行时，可在同系统、同架构的另一台电脑上导入并启动，无需联网补装组件。
[下载安装](https://github.com/chelal233/dsh-nexus/releases) · [用户指南](docs/user-guide.md) · [常见问题](docs/faq.md) · [文档目录](docs/README.md)

![Nexus 工作台](docs/images/workbench-zh.jpg)

*v0.1.8 工作台，使用尚未安装 Harness 的隔离首次使用环境。不支持的 Desktop 选项不会显示。查看[全部截图与采集说明](docs/images/README.md)。*

## 你可以用 Nexus 做什么

- **使用 Web 或官方桌面端**：通过系统浏览器打开 Web Harness，或启动受支持的受管 Harness 版本自带的原生 Desktop。Nexus 检查可用性后才显示桌面选项，不再维护替代客户端。
- **自己决定版本和配置**：新版本先准备再切换，默认最多保留 8 个受管版本槽位；在工作台切换 Web 配置档。官方 Desktop 在自己的窗口中管理配置，也可以关联已有的已构建 Harness 目录。
- **启动失败后有明确的下一步**：区分出错插件、缺失服务的提供方和仍在等待的插件。查看建议，在适用时暂时禁用已定位的相关插件，再检查并重试。插件包和数据保留，停用后可以重新启用。
- **看清运行状态并直接操作**：工作台与托盘提供启动、打开、中止、配置和维护入口。退出时可选择仅关闭 Nexus，或先中止全部服务；准备过程显示阶段、耗时和取消入口。
- **离线迁移完整环境**：将 Harness、匹配的运行时和选定数据一起导出，在同系统、同架构下导入完整包，无需联网安装依赖。仅配置或仅数据导出属于部分包。
- **自主选择插件与通知**：选择插件市场，也可以不使用市场；按事件、通知方式和窗口焦点设置任务提醒。可用事件取决于 Harness 及其监听插件。
- **使用中文或英文**：界面、使用文档和发布日志均提供内容对应的两种语言。

启动检查不只判断进程运行或网页可访问，还检查客户端插件与核心服务，区分检查中、功能受限、失败和未验证状态。这不代表已验证每次会话、工具调用或运行中的业务。Nexus 不替代 Harness 的 Agent、会话和模型能力；更新 Nexus 与切换 Harness 是两个独立操作。

## 下载与安装

| 平台 | 架构 | 下载文件 | Web Harness | 官方 Desktop |
| --- | --- | --- | --- | --- |
| Windows | x64 | EXE 安装包、ZIP 免安装包 | 支持 | 所选 Harness 版本支持时可用 |
| Windows | ARM64 | EXE 安装包、ZIP 免安装包 | 支持 | 暂不支持 |
| macOS | Intel x64 | DMG、ZIP 应用包 | 支持 | 所选 Harness 版本支持时可用 |
| macOS | Apple Silicon ARM64 | DMG、ZIP 应用包 | 支持 | 所选 Harness 版本支持时可用 |
| Linux | ARM64 | AppImage、DEB、RPM | 支持 | 暂不支持 |

[v0.1.8](https://github.com/chelal233/dsh-nexus/releases/tag/v0.1.8) 的五个平台目标均已通过 CI 与安装包检查。支持范围遵循 Harness 上游与 Electron 的交集；本版本不提供 Linux x64 或 x86 32 位安装包。官方 Desktop 是否可用还取决于所选 Harness 版本。

Linux ARM64 上，兼容的 Debian 系统可选择 DEB，兼容的 RPM 系统可选择 RPM，支持 AppImage 的环境也可使用 AppImage。包格式相同不代表已兼容所有 deepin、统信 UOS、麒麟等发行版及其版本，还需满足系统库和桌面环境要求。CI 与安装包检查不等于逐一验收所有发行版和 Harness 业务场景。

按 Release 附件及其 `_build.json` 记录识别构建。签名状态以具体附件为准，参见[安全说明](SECURITY.md)。

Windows ZIP 必须完整解压，再运行目录内的 `Nexus Launcher.exe`，不能单独复制 EXE。免安装不代表所有用户数据都存放在解压目录。macOS 将完整应用复制到合适位置后启动。

## 三步开始

1. 打开 Nexus，确认其后台 Agent 在线。
2. 安装需要的 Harness 版本、导入完整离线包，或关联已经构建完成的外部目录。
3. 使用 Web 时，确认数据目录与配置档，处理启动检查阻断项后启动并在浏览器打开；支持官方 Desktop 时，可选择桌面端并在其窗口中管理配置。

## 离线使用与启动准备

对于受支持的受管版本，联网取得所选 Harness 后，匹配的运行依赖由 Nexus 提供。本地准备不联网补装 Desktop 依赖。Nexus 与官方 Desktop 复用兼容的 Electron 运行时，后续启动复用已校验的本地文件，减少重复占用和准备工作。

需要断网迁移时，从准备完成的环境导出**完整离线包**，在同系统、同架构下导入。仅配置／数据的部分包不能代替完整包。使用外部源码目录时，须自行预先构建完成；Nexus 不代为编译或安装开发工具链。

离线启动不意味着远程模型服务、插件下载或其他联网功能也能离线使用；除非所选服务本身在本地运行，这些功能仍需要网络。

## 程序与数据分开

| 内容 | 行为 |
| --- | --- |
| Nexus 程序 | 安装包或完整解压目录 |
| Nexus 数据 | 保存配置、版本槽位、操作记录和诊断；独立于程序目录 |
| Harness 数据 `DSH_HOME` | 可选择路径；改路径不迁移或删除已有数据 |
| 外部 Harness | Nexus 检查并启动，不替你更新、编译或删除源码 |
| 项目工作区 | 由 Harness 管理，不等于程序目录或 `DSH_HOME` |

## 文档与参与

- 使用：[用户指南](docs/user-guide.md)、[配置说明](docs/configuration.md)、[故障处理](docs/troubleshooting.md)。
- 开发：[贡献指南](CONTRIBUTING.md)、[架构](docs/architecture-baseline.md)、[发布流程](docs/github-release.md)、[插件说明](plugins/README.md)。
- 状态：[变更记录](CHANGELOG.md)、[已知限制](docs/known-limitations.md)、[历史证据](docs/history/README.md)。

Nexus 是独立项目，不代表上游 Harness 官方发布。自有代码采用 [MIT](LICENSE)，第三方组件遵循各自许可证，见[许可材料](THIRD_PARTY_NOTICES.md)。

## 社区

- 感谢 [LINUX DO](https://linux.do/) 社区提供开放、友善的技术交流平台。
