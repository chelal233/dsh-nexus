# Nexus 内置插件

[English](README.en.md)


[返回文档目录](../docs/README.md)

## 模块与职责

每项扩展能力独立放在 `plugins/<plugin-name>/`，各自维护包清单、Host/Client 入口与测试。
Launcher/Agent 负责部署、生命周期、授权通信和原生能力；业务事件识别属于对应插件。
插件通过 Harness 的 Cordis 组合机制装载，不修改上游源代码。

Loader 补丁的 `name` 必须是 JSON 字符串，并使用经过转义的 `file://` 模块 URL；不能序列化 Rust `OsString` 或直接传 Windows 盘符路径。CLI 上下文中的 `entry` / `pnpm` 则使用普通路径字符串。`generated_plugin_json_uses_string_paths_and_loads_in_upstream_loader` 验证实际生成文件；设置 `NEXUS_TEST_DSH_ROOT` 后还会通过该 Harness 的真实 Loader 装载三个内置插件。

- `nexus-notifications`：任务事件监听与终端提醒。
- `nexus-desktop-bridge`：Web 客户端启动报告、健康检查及受能力检测保护的目录选择、通知定位和 `desktopWindow` 接口。普通系统浏览器不具备 Electron preload，不能宣称原生接口或真实拖放路径始终可用。
- `nexus-desktop-compat`：提供 `desktopProfiles.current/list/select` 和 `desktopPnpm.run/runPlugin/runExternalMarketPluginInstall`。切换由 Agent 执行，包管理由 Harness 的 subprocess 服务持有，等待整个进程树退出才释放操作。

兼容层不包含上游 desktop 包的运行时模块，也不承诺私有接口。插件应按服务注入/能力检查使用公共接口。Nexus 使用真实 Node ABI，普通 `runPlugin` 保留 GitHub、本地包等 Harness 原生支持的来源；不会强制使用精确 npm 版本入口。诊断探针禁止执行包修改和 Profile 切换。

选择 `dshmarket` 时，临时补丁声明其对 `desktopProfiles` 的装载依赖，避免模块加载时序使它误入自行重启分支。不会改动用户 Profile 的市场配置或上游包内容。

当前通知插件由 Agent 随包携带，在启动时部署到 Nexus 管理的运行目录并以 `--patch`
挂载到所选 Harness；不会改写用户 Profile 的依赖清单。设置中可关闭该插件，下次
Harness 启动生效。正式插件安装器接入前，此部署方式不是 npm 安装，也不向用户
Profile 的 `node_modules` 写入文件。

这些兼容插件用于 Nexus 管理的 Web 启动链路。官方 Desktop 使用自己的上游插件与原生服务，Nexus 不向其注入 Web 兼容补丁；不能由 Web 模式的接口清单推断 Desktop 插件支持。
