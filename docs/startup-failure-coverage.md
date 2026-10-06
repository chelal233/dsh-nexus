# Nexus 启动故障处理边界与覆盖

[English](startup-failure-coverage.en.md)

最近核对：2026-10-05，Harness 最新已发布版本 [dsh-v0.2.1-alpha.1](https://github.com/deepseek-ai/deepseek-harness/releases/tag/dsh-v0.2.1-alpha.1)（预发布，2026-10-03 发布），固定提交 `5badb15009ae1756c3afe0ae0cef1faafc290ccc`；本地实现为 Nexus v1.0.6 启动检查器 v17。

本轮核对新版启动、配置加载、CLI 和 Desktop 相关源码，并对照现有分类规则与回归用例。源码核对与模拟回归不代表此版本的真实升级、完整会话迁移或全部第三方插件已经验收。

Nexus 内置 Desktop 离线运行时锁仍为 Harness `0.1.6-alpha.2`。它是随包交付版本，不是本文的最新诊断核对基线；本轮不改运行时锁或用户选用的 Harness。

## 职责与成功标准

Nexus 负责启动前检查、进程启动、插件加载、服务依赖、连接建立和 Web 客户端启动验收。官方 Desktop 的内部就绪状态由其客户端呈现，进程存在不能等同于 Web 客户端就绪。运行后仅观察进程退出和连接状态；模型请求、工具调用、会话业务和审批流程由 Harness 负责。

MCP 等组件在初始化阶段阻止启动时属于本范围；启动后的单次调用失败不属于本范围。不能通过扫描任意运行日志里的 `error` 或 `timeout` 就宣告启动失败。

Web 成功需分别证明：本次配置检查通过、当前进程可连接、当前运行对应的客户端报告就绪。进程存在、HTTP 可访问、历史成功报告都不能单独证明成功。未知状态必须保持“未验证”。

## 最新版本核对与待验收项

| 新版行为 | 当前识别或处理边界 |
|---|---|
| 必需插件失败采用 `startup failed`、`Failed plugins`、`Plugins waiting for services` 分组输出；可选失败保留逐条告警 | v17 保留这些文本格式，区分失败包与等待服务；等待消费者不能自动等同于故障提供方 |
| `loadProfileDirectory` 收集无效或不兼容组合包到 `skippedBundles`，`reportSkippedBundles` 输出跳过原因 | v17 有界采集原生输出，保留包名、原因和截断标记；认证就绪仍显示受限，独立致命错误仍阻断。当前运行日志须匹配进程身份；客户端未验证时不能称就绪。Canary 遇到跳过项保持未确定，不把未加载插件算通过 |
| 原生配置加载通过 `normalizeShippedProfile` 和 `dropRetiredBundles` 规范化受管组合并移除已退役组合包，符合条件时写回清单 | 独立检查在隔离副本中执行，记录变更文件及前后 SHA256，不回写原配置。受管 CLI 实际启动前备份原 `package.json`、`cordis.patch.yml`（包括缺失状态），可从“维护 → 配置修复”恢复；备份失败拒绝启动。新建配置不由 Nexus 代建。自动回归与受管进程集成已验证此保护，不等于最新版全平台真实升级 |
| 上游移除运行时 invariant 插件及 `./invariant` 导出，并改变插件子路径的元数据读取方式 | 模块缺失、导出接口错误有通用分类；保留原始导入路径。第三方导入及子路径声明的逐插件适配仍需核对，不自动恢复已删除的接口 |
| 未打包 Desktop 仍要求有效目标及 `DSH_DESKTOP_PRIMARY_RUNTIME_DIR` | 目标、准备资源和环境变量是启动输入；核对 Nexus 的准备流程及原始报错，不能据此停用任意用户插件。最新版本真实 Desktop 准备与启动未在本轮验收 |
| Desktop Host 默认以 `--port 0` 启动，经 IPC 交付实际认证 URL | 不用固定端口判断官方 Desktop 是否就绪。启动、子进程就绪和窗口/客户端状态仍需分别验证 |
| Web 新增 `--public-url`，支持对外地址及路径前缀 | 此项依据新版发行说明跟踪；反向代理与自定义公开地址未在本轮验收，不能把公开地址当成本地认证或当前客户端就绪证据 |

固定提交的源码依据：[启动政策与分组报告](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/packages/boot/app-boot/src/index.ts)、[配置档与跳过组合包](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/packages/boot/app-boot/src/profile.ts)、[依赖解析](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/packages/boot/app-boot/src/profile-resolution/resolver.ts)、[Loader 补丁目标](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/vendor/loader/src/config/tree.ts)、[CLI 参数](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/cli/src/args.ts)、[Desktop 目标](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/desktop/scripts/desktop-build-paths.mjs)、[Desktop 准备](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/desktop/scripts/development-project.ts)、[Desktop 主进程](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/desktop/src/main.ts)、[Desktop Host 端口与就绪](https://github.com/deepseek-ai/deepseek-harness/blob/5badb15009ae1756c3afe0ae0cef1faafc290ccc/apps/desktop-host/src/index.ts)。破坏性变更与 Web 对外地址见上述版本的官方发行说明。

## 当前覆盖

“处理”包括准确提示和明确的人工出口，不等于自动更改配置。

| 阶段/情况 | 当前识别与处理 | 验收条件/限制 |
|---|---|---|
| 运行环境、入口、版本、路径 | 基础检查定位缺失项，进入版本选择或运行环境设置 | 修复后重新检查；自定义命令仅按可验证能力检查 |
| 配置、参数、配置档 | 配置读取/解析、保留配置名、无效启动参数分别说明 | 配置/补丁错误可打开对应文件；端口和参数错误进入设置，不再误导到补丁 |
| 数据目录和文件权限 | 读取/写入权限、访问拒绝、锁定相关证据 | 用户处理指定路径权限/占用；不自动提权或删除数据 |
| 磁盘不足 | 启动失败中的 ENOSPC 独立识别 | 释放对应磁盘空间再检查；不自动删除会话 |
| 端口占用 | 基础检查和 EADDRINUSE 证据 | 修改端口或处理已确认的占用程序；不猜测并终止其他程序 |
| 依赖和接口不兼容 | 缺失模块、包声明、导出接口、模块布局、补丁目标、重复条目分类 | 修复对应依赖/版本/补丁；声明兼容不等于真实加载通过 |
| 插件加载失败 | 原始失败条目与等待条目分开；精确归属到当前配置中的包 | 只有证据支持时提供停用建议，停用后重新走检查 |
| 官方插件等待服务 | 即便没有可停用插件，也保持启动失败 | 查找提供方，不能把消费者当责任插件；静态替换证据并非完整服务依赖图 |
| Nexus 自带组件加载失败 | 单独识别已知 Nexus 加载错误，不生成第三方停用建议 | 修复/更新 Nexus 安装，查看原始日志 |
| 进程提前退出 | 无输出退出保留退出码；有输出时保留根因证据 | 退出本身不证明哪个插件有故障 |
| 启动超时 | 仅识别启动探测超时，保留最后输出，优先解释具体根因 | 不把任意组件超时当作整体就绪超时；不无限重试 |
| 探测进程停止超时 | 与启动就绪超时区分 | 确认上次进程已停止后再检查 |
| 检查期间配置变化 | 标记输入变化并要求重新检查 | 不据旧报告建议修改插件 |
| 可选插件告警 | 按上游启动政策保留可用但受限状态 | 若官方依赖仍等待服务，则不能仅凭 HTTP 可用放行 |
| 客户端加载失败、无响应、无报告 | 区分检查中、未验证、缺少服务、插件阻塞与就绪 | 报告必须匹配当前运行；无报告不能确认失败插件 |
| 恢复/安装事务未结束 | 基础检查阻止冲突启动，进入相应恢复/维护入口 | 事务完成或按现有流程撤销后重新检查 |
| 未知错误/新版本输出 | 保留原始错误及日志入口，不推断责任插件 | 人工诊断或更新适配；不得显示“已修复” |

## 现有回归与验证边界

- 兼容上游逐行告警和分组致命启动报告，保留失败包归属与等待服务；表格未提供的包归属不作推断。
- 修正官方服务等待但无停用候选时被视为通过的情况。
- 基础原因优先于配置包装错误和插件等待症状；针对权限、端口、磁盘问题不生成停用方案。
- 补充 Nexus 集成失败、磁盘不足、配置变化、清理超时和静默退出的说明。
- 超时保留诊断尾部；普通组件 `timed out` 不直接匹配启动就绪超时。
- 跳过组合包在认证就绪、独立失败和缓存复用中保留；大量后续日志不会丢失已采集的跳过证据，输出长度和多语言字节数受限。
- 已确认官方就绪协议的受管 CLI 在加载组合包前预加载有界观察器；官方 `appReady` 提交将跳过摘要封存到本次宿主记录。后续日志增长不会抹掉该摘要，提交后的后台输出不纳入启动证据。Agent 校验运行标识、可归属的 PID 和数据边界，界面再校验本次 generation 与认证客户端；未知启动协议仍使用普通日志观察。
- 原生清单调整仅发生在检查副本；真实受管 CLI 启动前的配置备份、精确恢复及备份失败阻断均有隔离回归。编辑和启动前备份仍限每文件 32 KiB；检查及恢复另设 256 KiB 上限，以便恢复被原生格式化扩大的清单。覆盖前仍备份当前内容并核对指纹，超过恢复上限则拒绝覆盖；不备份整个会话库，也不自动回退配置。原生日志的多行原因只保留可确认的首行，无法归属的后续文本标明证据不完整，不作为插件原因。
- 修复端口、权限和参数错误的界面导航。
- 自动回归使用临时配置和模拟启动进程，不修改用户插件，也不制造磁盘满、真实文件破坏或生产故障。

## 跟随上游更新的维护规则

- 每次核对官方新 Release 时，记录日期、tag、是否预发布及固定提交；最新预发布与稳定渠道分别标注，不以漂移的 `master` 代替发布依据。
- 对照启动输出、配置/组合包解析、CLI、Desktop 准备及就绪协议，更新本页与中英文故障处理文档，并同步分类器的来源注释。
- 将“现有规则仍适用”“需要代码适配”“尚未实测”分别记录。新增识别规则必须有对应回归；源码核对不能写成真实升级通过。
- 保留旧版输出的兼容规则；内置运行时锁和历史验收证据单独记录，不让它们取代最新版本的核对。

## 尚不能承诺的能力

- 目前依据已核实的上游输出和有限结构化报告分类，并非对所有异常类型的完备解析。
- 服务提供方定位仍依赖声明和明确替换证据，不能仅根据服务名称还原任意第三方依赖图。
- 版本损坏、权限、网络和第三方代码缺陷不一定可自动修复；Nexus 应提供准确出口和复检，不能承诺一键解决。
- 启动成功不保证所有会话、模型或工具功能正确。Nexus 不读取业务会话内容来扩大诊断范围。

## 实现入口

- 基础检查：`crates/nexus-agent/src/preflight.rs`
- 启动探测与分类：`crates/nexus-agent/src/compatibility.mjs`
- 错误签名与处理分类：`crates/nexus-agent/src/startup-diagnosis.mjs`
- 进程监管：`crates/nexus-agent/src/supervisor.rs`
- 客户端启动观察：`apps/nexus-launcher/electron/client-audit.mjs`、`plugins/nexus-desktop-bridge/client.js`
- 修复入口与执行：`apps/nexus-launcher/src/views/startup.tsx`、`startup-repair.tsx`
- 验证：`crates/nexus-agent/tests/compatibility.test.mjs`、Launcher 启动界面回归测试
