# 贡献约定

Nexus 的定位是标准 Harness 启动器。提交修改前，请在 Issue 说明具体用户场景、现有行为与预期行为；修复明确缺陷的小改动可直接提交 PR。

- 不修改上游 Harness 源码，不自动迁移用户数据。
- 外部 Harness 程序目录由用户维护，Nexus 不对其安装依赖、构建、更新或删除。
- Nexus 安装、升级、卸载不得要求客户机现场编译；受管 Harness 首次安装构建是独立流程。
- 复用现有权限、事务和错误码机制；保留原始错误和失败证据。
- 同步简体中文/English 文案；避免将密钥、完整用户配置或诊断数据放入测试夹具。
- 回归测试使用临时目录与自己拥有的进程，不能操作开发者真实配置、外部 Harness 或未知进程。

从仓库根目录进入 `apps/nexus-launcher`，执行 `pnpm install --frozen-lockfile`。修改后执行 `pnpm format`、`pnpm typecheck` 和相关测试。CI 与本地发布门禁均执行格式检查；TypeScript 会拒绝未使用的局部代码和参数。涉及跨层契约时检查 Rust workspace、原生桥接和 UI。完整命令见 [README](README.md)。

## 按功能定位代码

| 要修改的行为 | 入口 |
| --- | --- |
| 页面切换、全局轮询、动作串行化 | `apps/nexus-launcher/src/App.tsx` |
| 工作台、设置、版本安装、资料、恢复、维护、启动检查 | `apps/nexus-launcher/src/views/` 中同名功能文件 |
| 原生/浏览器请求传输、请求重试 | `agent-bridge.ts`、`request-client.ts` |
| 公共组件、数据读取、状态文案 | `ui-components.tsx`、`json-values.ts`、`display-format.ts` |
| Agent 启动、路由、共享运行状态 | `crates/nexus-agent/src/lib.rs` |
| Profile 选择、插件、终端 | `crates/nexus-agent/src/profile_api.rs` |
| 配置写入、响应脱敏 | `crates/nexus-agent/src/config_api.rs` |
| 快照恢复、重试、撤销及事务恢复 | `crates/nexus-agent/src/checkpoint_api.rs` |
| Agent 的权限、快照与切换回归 | `crates/nexus-agent/src/tests/` |

前端相对文件名以 `apps/nexus-launcher/src/` 为基准。功能页面可以调用公共模块；公共模块不能反向依赖页面，页面也不能导入 `App.tsx`。模块依赖测试会拒绝循环引用。组件测试通过 `tests/ui-test-entry.ts` 加载真实组件，新增测试导出放在这里，不给应用入口添加测试专用导出。

先删除无调用代码，再复用已有函数；只有职责独立时才拆分模块。保持显式导入，避免通配导入、通用注册器和仅转发调用的包装层。一个判断或副作用单独书写，注释解释原因与不变量。事务的加锁、持久化、子进程所有权和失败恢复顺序属于行为契约，不能为了缩短代码省略。

## 保持行为的 A/B 对照

保持行为的修改必须先保存修改前 A 基线，再以同一组输入对照修改后 B 的输出、错误和副作用。不能用“两边的单元测试都通过”代替直接对照，也不能为消除差异而更新 A。类型修改还应比较擦除类型后的 JavaScript。保存基线与候选哈希、对照结果和未覆盖范围；有差异时先解释并处理，再交付。

在 `apps/nexus-launcher` 中，可用 `pnpm test:ab capture ../../target/ab-<唯一名称>` 保存当前前端源码，再用 `pnpm test:ab compare ../../target/ab-<唯一名称>` 对照。当前脚本覆盖健康检查、GET/POST 桥接、Harness 会话匹配、动作执行（含错误收尾、预检反馈、防重复点击）和全局刷新（含并发合并、离线、忙碌及凭据失效），遇到范围外的源码改动会拒绝通过，必须先补对应的 A/B 场景。原生、事务和安装器改动需要各自的本地对照环境；此脚本不证明它们等价。基线和结果保存在 `target`，不提交真实用户数据。

快照恢复的本地 A/B 探针是 `apps/nexus-launcher/tests/ab-checkpoint-probe.rs`。在独立的 `target/ab-<名称>/rust/A`、`rust/B` 源码副本中，将同一探针追加到 `crates/nexus-agent/src/tests/checkpoint_tests.rs`，分别执行 `cargo test -p nexus-agent --lib --locked ab_checkpoint_observations -- --test-threads=1`，用 `NEXUS_AB_REPORT` 指定各自的 `rust/A.json`、`rust/B.json`。随后在前端目录执行 `node scripts/ab-checkpoint.mjs ../../target/ab-<名称>`；对照器核验编译来源与实际源码后比较响应、目录元数据、恢复日志及运行状态。只归一化夹具路径、生成的检查点 ID 和明确的时间戳字段；该探针不编入发布程序。

PR 请说明触发条件、行为变化、验证结果和未验证范围。静态阅读、自动化测试、模拟进程联动与真实安装验收分别列出，跳过的测试不能标记通过。

不要提交 `target`、安装包、运行日志、诊断目录、访问凭据或本机专用路径。发布签名、自更新、多用户服务与完全离线首次安装不属于当前实现范围；新增此类能力应先讨论。
