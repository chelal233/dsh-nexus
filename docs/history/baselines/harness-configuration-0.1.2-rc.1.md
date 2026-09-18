> **历史归档 / Historical archive** — 保留原始记录，版本、路径、链接与结论可能已过时。Original evidence is preserved; versions, paths, links and conclusions may be obsolete.
> [归档目录 / Archive index](../README.md) · [当前文档 / Current documentation](../../README.md)

# Harness 可配置项：0.1.2-rc.1

核对日期：2026-09-07。依据本机已安装的上游 `@deepseek-ai/dsh-root` 0.1.2-rc.1 源码及默认 bundles，不代表其他版本或任意第三方插件。未读取用户实际 `.env`、凭据或会话内容。

源码根目录：`C:/Users/PC/AppData/Local/Nexus/releases/harness-dsh-v0-1-2-rc-1-1788609463088298700`。下文证据路径均相对此目录。

## 目录要分开看

| 目录 | 用途 | 能否改变 |
|---|---|---|
| Nexus 程序安装目录 | EXE、内嵌 Node/npm/pnpm | 属于安装器设置，不是 Harness 参数 |
| Nexus 数据目录 | 下载、版本槽、诊断、Nexus 状态 | Nexus 自己的 `NEXUS_DATA_DIR`，不是上游 Harness 变量 |
| Harness 数据目录 | profiles、持久配置及相关数据 | 上游支持 `DSH_HOME`，默认用户目录下 `.dsh` |
| 项目工作目录 | 当前会话处理的项目和文件 | 与数据目录不同；Web 应沿用会话/工作区选择机制 |

`DSH_HOME=D:\AIData\Harness` 是有效的选择方式，目录不必叫 `.dsh`。Nexus 可保存选择后，通过启动环境传给 Harness。目录读取、插件管理、兼容性检查、快照也必须使用同一位置，不能只改 Node 子进程。

首次使用内置 profile 时，上游 `loadProfile()` 会调用 `initProfile()`，递归建立 profile 目录和缺失配置。源码：`packages/boot/app-boot/src/profile.ts:196,805`。本次 Nexus 首启缺陷发生在它执行之前。

## 正式启动参数

以下 `dsh` 是上游命令名；Nexus 内部使用内嵌 Node 执行已安装版本的 CLI 入口，用户不需要另装全局命令。

| 参数或命令 | 用途 | 默认/边界 |
|---|---|---|
| `--profile <name>` | 选择 profile | 根命令要求指定；`web` 子命令等同选择 web |
| `--patch <path>` | 追加一份配置补丁 | 可重复；放在应用参数之前 |
| `--dump-config` | 输出合成配置后退出 | 诊断用途，不启动应用；包含用户配置层 |
| `--dump-default-config` | 输出 bundle 配置后退出 | 不含用户层；不能同时传 `--patch` 或应用参数 |
| `-V` / `--version` | 查看版本 | 信息命令 |
| `-h` / `--help` | 查看帮助 | 裸命令显示入口帮助；选择 profile 后由具体应用解释 |
| Web：`--port <number>` | 监听端口 | 默认 bundle 为 3080；0 表示系统分配空闲端口 |
| Web：`--host <host>` | 监听地址 | 默认 127.0.0.1；本版本明确拒绝 0.0.0.0 |
| Web：`--no-open` | 不自动打开浏览器 | 默认尝试浏览器交接；SSH 情况会跳过交接 |
| Web：`--trusted-host <authority...>` | 增加浏览器访问的受信 Host/Origin authority | host 或 host:port；不是改变监听地址，也不是取消认证 |
| Headless：位置参数 `<task...>` | 一次性任务文本 | 多个词拼接；空任务报错 |
| `plugin --profile <name> <pnpm 参数>` | 管理该 profile 的插件 | 例如 add/remove/why；不是通用启动开关 |

已确认内置 profile 模板：`web`、`headless`、`acp`、`sdk`、`sdk-minimal`。帮助中的 `tui` 示例不意味着此版本附带同名内置模板。

入口在第一个不认识的参数处转交给所选应用。因此 `--resume`、`--model`、`--cwd` 等不能仅凭其他产品经验当作全局参数；Web 的正式参数列表中没有这些选项。

例子：

```text
dsh --profile web --no-open --port 0
dsh --profile web --patch D:\AIConfig\web.patch.yml --port 8080
dsh --profile headless "运行这个项目的测试并说明结果"
```

证据：`apps/cli/src/args.ts:116-184`；`packages/bundle/web-app/src/startup.ts:46-89`；`packages/bundle/headless/src/startup.ts:31-55`；`packages/boot/app-boot/src/profile.ts:134`。

## 已确认的运行环境变量

环境变量通常在启动时读取。修改后应重启相关进程；不要假设能热更新。下面不包括构建、测试变量，也不把第三方 SDK 的所有常见变量推断为默认支持。

| 环境变量 | 用途 | 默认值/适用范围 |
|---|---|---|
| `DSH_HOME` | Harness 数据根目录 | 默认 `~/.dsh`；Nexus 当前要求绝对路径 |
| `DEEPSEEK_API_KEY` | 官方模型与官方搜索密钥 | 未设置时由凭据服务等来源解析；插件可用 `apiKeyEnv` 改引用名称 |
| `DEEPSEEK_BASE_URL` | 官方模型 API 地址 | `https://api.deepseek.com`；显式插件 `baseURL` 优先 |
| `DEEPSEEK_SEARCH_BASE_URL` | 官方搜索 API 地址 | `https://api.deepseek.com/anthropic/v1`；与模型地址独立 |
| `EXA_API_KEY` | Exa 搜索密钥 | 需要相应插件已加载；无密钥则该插件不可用 |
| `PERPLEXITY_API_KEY` | Perplexity 搜索密钥 | 同上 |
| `DSH_WEB_SEARCH_PROVIDER` | 选择已注册的搜索提供者 ID | 显式插件配置优先；未指定且仅一个可用者时自动选择 |
| `DSH_WEB_FETCH_PROVIDER` | 选择已注册的抓取提供者 ID | 同上；多个可用者不是自动依次回退，而是要求明确选择 |
| `DSH_TELEMETRY_MODE` | 遥测模式 | `FULL` / `FEEDBACK_ONLY` / `DISABLED`；默认 base 为 `FEEDBACK_ONLY` |
| `DSH_TELEMETRY_DISABLED` | 关闭遥测 | **任何非空字符串都关闭**，包括 `0` 和 `false`；不是普通布尔解析 |
| `DSH_TELEMETRY_OTLP_URL` | 遥测接收地址 | 默认 `https://harness-telemetry.deepseeksvc.com/v1/logs` |
| `DSH_PERMISSION_MODE` | base 层权限/隔离默认模式 | 默认 `workspace-write`；`danger-full-access` 同时使 base 默认审批为 never；会话权限预设仍可覆盖，不能视为所有 Web 会话的无条件全局设置 |
| `DSH_AGENTS_HOME` | 共享 agent 技能根目录 | 默认 `~/.agents`；显式插件 `agentsHome` 优先 |
| `DSH_BUNDLED_SKILL_DIR` | 额外 bundled 技能根 | 需启用默认技能根；显式插件配置优先 |
| `DSH_TOOLS_MODE` | 工具调用模式 | `native` / `ptc` / `both`，默认 native；上游注释标为临时接口 |
| `DSH_CONTEXT_WINDOW` | SDK 最小配置的上下文窗口 | 默认 1,000,000；**仅 sdk-minimal**，不是 Web 的全局窗口设置 |
| `DSH_SYSTEM_PROMPT` | SDK 最小配置的系统提示 | 默认 `You are a helpful software engineer assistant.`；**仅 sdk-minimal** |
| `DSH_MAX_TOKENS_AS_SUCCESS` | 是否将达到 token 上限视为成功 | 默认 true；值用 JSON.parse；**仅 sdk-app** |

证据索引：

- home：`packages/util/home-paths/src/index.ts:79-111`
- 官方模型：`packages/llm/llm-deepseek/src/index.ts:125-163,199-203,377-380`
- 官方搜索：`packages/web/web-search-deepseek/src/index.ts:43-50,82,109-112`
- 搜索插件：`packages/web/web-search-exa/src/index.ts:36,61`；`packages/web/web-search-perplexity/src/index.ts:31,56`
- provider 选择：`packages/web/web/src/index.ts:63-93`
- 遥测/权限：`packages/bundle/base/cordis.patch.yml:168-196,209-244`；`apps/cli/src/profile-boot.ts:89-103,170`
- 技能：`packages/skill/skill-filesystem/src/index.ts:48-74,164-172`
- 工具模式：`packages/bundle/web-app/cordis.patch.yml:33-37`；`packages/bundle/headless/cordis.patch.yml:15`
- SDK：`packages/bundle/sdk-minimal/cordis.patch.yml:30,96`；`packages/bundle/sdk-app/cordis.patch.yml:21`

## `.env` 与启动环境不是一回事

上游发现并加载的 `.env` **禁止**包含所有 `DSH_*`，也禁止 `DEEPSEEK_BASE_URL`、`DEEPSEEK_SEARCH_BASE_URL`、代理变量、`NODE_OPTIONS`、PATH/HOME 等启动或信任相关变量。违规会报错。Nexus 若开放这些设置，应在启动进程时注入，不能直接把它们写入 `.dsh/.env`。

普通允许变量的优先级：父进程环境 > 启动工作目录 `.env` > `$DSH_HOME/.env`。

密钥的优先级：父进程环境 > 保存的凭据文件 `.credentials.yaml` > 启动工作目录 `.env` > `$DSH_HOME/.env`。启动环境里的密钥在 Harness 内是只读来源；在凭据界面保存的值可覆盖旧 `.env`，但不能覆盖启动环境。

证据：`packages/boot/app-boot/src/index.ts:95-130,159-201`；`packages/credentials/credentials-local/README.md:69-81`。

`HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY` 被要求来自父进程，并不证明全部 HTTP 客户端都会采用它们。官方模型实现使用 raw fetch，没有统一代理配置；代理是否有效需要对内嵌 Node 和具体提供者实测，不能承诺“一项设置覆盖所有网络请求”。

`DSH_WEB_URL` / `DSH_WEB_MODE` 是运行时发布的结果，不是用户填写的输入。`DSH_BUILD_*`、`DSH_CLIENT_*`、测试/snapshot/experimental 变量也不应列为稳定设置。

## 配置文件能调整更多内容

环境变量只是入口的一部分。上游有 117 个插件配置条目的生成目录：

[上游完整插件配置目录](<C:/Users/PC/AppData/Local/Nexus/releases/harness-dsh-v0-1-2-rc-1-1788609463088298700/docs/config-catalog.md>)

目录列出了声明类型；其中标为 runtime-only、且不被运行时 schema 接受的字段，不能作为配置文件选项。插件还必须实际加载并满足所需服务，字段才有作用。

可进一步配置的主要领域包括：模型/提供者、thinking effort、最大输出、流空闲超时、重试、搜索/抓取、技能目录和监听、MCP、shell/sandbox、会话存储、工作区、Web 服务及前端插件等。

通常通过 home/profile 的 `cordis.patch.yml`、追加 `--patch` 或 Harness 已有设置界面调整。热更新能力取决于 profile 和具体插件：内置 web profile 的 patchReload 是 live，其他上述内置模板为 startup；不能把“文件能改”当作“所有字段立即生效”。

## 适合 Nexus 开放的设置

第一批：Harness 数据目录、默认 profile、监听端口、是否自动打开浏览器、遥测关闭开关。保持已有默认值，先解决普通用户的安装与启动需求。

高级设置：官方模型/搜索 API 地址、搜索/抓取提供者、共享技能目录、额外配置补丁。权限模式要说明实际作用范围；PTC 和 SDK 专属项标出适用 profile，避免当成 Web 通用设置。

项目工作目录应使用 Harness 的会话/工作区功能，不能用 `DSH_HOME` 替代，也不宜直接改动 Nexus 当前用于启动版本槽的工作目录。程序、Nexus 数据、Harness 数据、项目文件可以分别归类。

Nexus 设置页现已提供上述选项。空文本、空补丁列表和“沿用上游默认行为”不形成覆盖；显式 false 和端口 0 有效。保存需先停止 Harness 并等待更新、清理及快照操作结束。默认 profile 复用现有选择接口，单独保存。

数据路径只改变访问位置，保存时不会创建、复制、移动或删除 Harness 数据；首次启动仍按原有流程初始化所指位置。清空后恢复环境变量或上游默认路径，不迁移数据。项目工作目录和 Nexus 启动版本槽目录保持原用途。

端口 0 由系统分配，Workbench 从本次启动日志读取真实网址；此时进程 Running 不等同于网页已就绪。权限模式是 Harness 内部默认工具策略，可被会话预设覆盖，不代表操作系统提权。SDK 项仅应用于注明的内置源 profile。
