updated: 2026-09-05 00:39 by ZCode (GLM, 接管 Codex 会话后的需求对账)

# dsh-nexus 项目状态与需求基线

## 运行模式定界（用户 2026-09-05 拍板）

- **Node 运行模式是唯一受支持/受测路径**：跟随 git 上游的概念——我们 clone 源码按 tag 运行，没有预编译 dsh_harness.exe 配置。运行时供给（node/pnpm）因此是全链路硬前置
- direct 模式现状：协议层代码暂时保留兼容，但不在测试矩阵与 UI 推荐位；**用户后续将自行移除 direct 模式（不在本次 plan 内），届时清理 direct 相关 config/发现/协议分支即可**

## 工作协议（用户 2026-09-05 拍板：9400 万 token 预算内完成全部基线）

1. **子代理分担重读**：大文件通读、跨模块侦察一律派 Explore/general 代理，只带回结论
2. **输出过滤**：构建/测试输出只取尾部或失败行，不整段进上下文
3. **精确编辑**：Edit 用唯一锚点；读文件用 offset/limit 片段读，不整读大文件
4. **合批测试**：安全的低风险改动合并后一次测试；只在触碰安全敏感面（journal/supervisor/原子写）或批验收前才全量重测。测试降频但批次验收（GUI 实测）不降
5. **上下文收敛**：每批结束把状态写入本文件后主动收敛，必要时切新会话续航（批间交接零成本）
6. **文档随做随更（用户要求，随时可接管）**：每完成一个功能同步更新本文件的状态/坐标段与 docs/architecture-baseline.md 对应章节，不攒到最后补写；交接标准=任何新线程只读本文件+架构文档即可继续
8. **能不下载就不下载（用户要求，全链路资源复用原则）**：一切资源解析顺序=用户已有（系统 PATH 上的 node/pnpm/git、已发现的本地 Harness）→ 本地已有（runtimes/ 已下载过的便携运行时、已装的 release 槽位）→ 弹窗确认后才下载。下载永远是最后手段，且同一资源只下一次（有缓存/复用判断）
7. **系统环境红线（用户要求）**：不破坏本机系统环境；运行时安装测试**仅允许"仅 Nexus"便携模式**（落 runtimes/，删目录即净），禁止在本机做系统级安装（MSI/winget/PATH 写入）的实测——系统级路径的正确性靠单测与代码审查覆盖，真机验证留给用户

## 项目定位（已与用户对账确认）

- 对接上游 https://github.com/deepseek-ai/deepseek-harness ，上游零改动（Immutable black box）
- **Launcher（apps/nexus-launcher，Tauri 2 + React）= 纯表现层**：窗口/托盘/主题/表单，经 Rust 侧白名单代理 Agent `/v1/*` API；可换 Electron，业务不受影响
- **Agent（crates/nexus-agent，127.0.0.1:3090 无头服务）= 业务执行层**：Harness 启停监督、release 槽位、checkpoint、diagnostics、config，全部业务所在
- profile = **Harness 自带概念**，Nexus 只持有名字、经 `{profile}` 占位符渲染进启动参数；不做独立数据层工作区隔离
- 使命：省掉用户配置 Harness 的时间 + 检查点/配置档切换/版本秒切

## 边界修订（用户已拍板 2026-09-05）

1. **允许读取并快照 `$HOME/.dsh` 中的声明式配置文件**（profile 清单 + `home/settings.yaml` + `cordis.patch.yml`）用于快照回滚；仍不碰会话/凭证/会话数据
2. 槽位容量：**可配置 N 个，默认 3**；满槽时拒绝并要求用户手动选择释放，绝不自动清理
3. **快照触发时机（2026-09-05 最终拍板）：健康启动自动轮转 N 个槽（默认 3）+ 保留手动 checkpoint 作为补充**（采纳 desktop 模型：不依赖用户记得打快照，坏掉时永远有最近 N 次健康状态可回）
4. 参考实现：`D:\dsh-local\dsh-desktop\dsh-plugin-desktop`（95 个 src 模块，v2.0.4）。恢复模式四标签页是该外壳自己实现的（非 Harness 自带）；"Failed to load plugins" 启动失败页才是上游 `@deepseek-ai/dsh-host-webserver` 渲染的，desktop 只是注入按钮

## 待办基线（用户 2026-09-05 全部确认）

### P0
1. 远端 tag 枚举：`git ls-remote --tags` 只读端点（不用 GitHub API，走本机 git/代理）+ Updates 页 tag 下拉
2. 槽位容量 N（默认 3）+ 满槽拒绝 + 用户手动选择释放
3. 一键切换编排：选 tag → 已装直切 / 未装自动 clone+构建+安装后切换；运行中提示先停止（沿用现有 no-implicit-restart 保护）
4. **运行时供给（2026-09-05 新增拍板，无参考实现，估 2.5~3 段）**：node/pnpm 缺失时**弹窗确认后按需下载**（Nexus 安装包恒定几 MB，绝不自带）。两种安装方式由用户选择，**便携版为默认推荐**：
   - **仅 Nexus（便携）**：node 便携 zip 落数据根 `runtimes/`（用户级、免管理员、删目录即卸载、不碰系统 PATH——仅注入 Harness 子进程 env）
   - **系统级**：node 走 MSI 静默安装/winget（需 UAC 提权）、pnpm 走官方用户级脚本。**装后不归我们管**：卸载/升级/修复全走系统自带渠道，Nexus 的"卸载干净"承诺只覆盖便携版
   - **冲突判断（装前预检，纯只读）**：扫描 PATH 已有 node/pnpm/git 版本并如实展示；已有 node 低于上游 `engines` 下限 → 推荐便携版；已有版本仍选系统级 → 冲突警示后放行（知情选择不拦死）；**装完钉死绝对路径进配置**，启动/构建/终端全用钉死路径，不依赖 PATH 查找顺序
   - pnpm 便携侧走 node 自带 corepack，按上游 `packageManager` 字段自动取版本，操作统一加 `--config.minimumReleaseAge=0`（镜像 desktop 策略，进程局部、永不改写用户 pnpm 配置）
   - **git 缺失仅弹窗引导（winget/官网），不自动安装**
   - 接线四处：更新任务预检、node 模式 Harness 启动、快照恢复后依赖物化（pnpm reconcile）、DSH 终端 env 组装。desktop 无此能力参考——它靠 ELECTRON_RUN_AS_NODE 白嫖 Electron 运行时
5. 恢复模式框架（四标签页：插件管理/回滚/切换Profile/诊断），检测插件加载失败等启动崩坏
6. 插件管理：读 profile 清单（`dsh.profile.bundles`）区分内置/可卸，卸载走官方 `dsh plugin remove`，不改上游
7. 快照升级：checkpoint 扩展为快照声明式配置文件（见边界修订1），界面显示 DSH 版本/插件数/配置文件数/大小，可浏览文件 + 一键回滚。**实现路线（已确认）：快照内容设计照抄 desktop 的 profile-checkpoint.ts**（有界文件清单 + 大小上限 + 类型/权限校验 + sha256 校验 + 临时文件→rename 原子替换），**安全外壳沿用现有两阶段 intent journal**（不降级为 desktop 的单 skip 标记）；触发 = 健康启动自动轮转 + 手动创建。原估 2~3 段已压缩至 1~1.5 段
8. **网络与下载源（2026-09-05 小白门槛分析新增）**：node 便携包与 pnpm registry 的下载源可在设置中切换（官方源 / npmmirror 国内镜像，校验和机制不变）；**GitHub（tag 枚举/clone）不代办网络问题**——失败时仅人话提示引导用户自行处理（"可能是网络问题，请配置代理或镜像"）

### P1
8. Profile 新建 + 一键切换 UI（暂不做删除；desktop 新建 = 安全 Web profile 不选中不重启）
9. 配置文件快速查看/打开（settings.yaml / Profile 补丁 / 插件清单 / Profile 目录）
10. 一键打开 DSH 终端（desktop 用约十来个环境变量组装：DSH_HOME、profile 目录、shim 路径等，需先梳理上游环境变量约定）
11. 桌面通知（Tauri 原生）：更新完成/Harness 启动失败等事件推送，含设置开关（镜像 desktop notification settings）
12. 首次运行向导：检测未配置 → 引导发现/选择 Harness
13. 崩溃自动留证：Agent 检测到子进程异常退出时自动快照日志到 diagnostics（现在只有手动收集）
14. 错误人话化映射：特征报错 → 一句人话 + 可点动作（重试/打开日志/进入恢复模式），挂接恢复模式兜底
15. **内部端口去固定化（2026-09-05 用户拍板，升级 desktop 的顺序重试方案）**：Agent 等内部监听默认 bind 端口 0 由 OS 从 ephemeral 段分配（内核保证零冲突），实际端口写进 run/agent.json，launcher/CLI 靠身份校验发现，全链路无感。3090/3091 等仅作显式覆盖项（NEXUS_AGENT_PORT / launcher.json），默认 auto。**铁律：内部端口永不出现在用户可见 UI/文案**——设置页无端口输入框，报错不含端口号；用户唯一接触的是 Harness 自带 Web 端口，且它只从日志读取（/v1/harness/ui 现有设计），从不假设具体数值
16. "像个正常软件"基本素养：托盘常驻 + 关窗最小化不退出 + 可选开机自启
17. 一键修复/重置：重置 Nexus 配置（可选保留 Harness 数据），兜住恢复模式罩不住的怪问题
18. 帮助入口：日志查看（复用 diagnostics）、常见问题、上游文档链接、用户可选日志级别（镜像 desktop log-level）

### P2
19. 磁盘/文件系统预检（仅 NTFS/ReFS，拒绝可移动/远程盘；保护原子替换语义与 NEXUS_DATA_DIR 选址；顺带做安装前磁盘空间预检）
20. projcache 损坏自愈（desktop 有实战自愈逻辑，等真实案例再跟进）
21. Nexus 自身自更新通道（现在只能更新 Harness，不能更新 Nexus；desktop 有完整检查→下载→换装）
22. Windows ACL 沙箱适配（上游 ACL PowerShell 执行器；需实测 Nexus 直启 Harness 是否缺失该能力）
23. 卸载体验：卸载时询问是否清理 runtimes/ 与 releases/ 槽位；.dsh 用户数据默认保留

### 维持排除/延后
- 代码签名（SmartScreen 警告问题，用户拍板"后续再说"，先用文档教程过渡）
- LAN HTTPS 入口（远程访问类）
- Desktop Market 插件市场/广告（用户的"未来思考"）
- Agent API 鉴权 token（文档标注未来协议变更）

## 当前仓库状态（2026-09-05 11:04 更新，runtime-fix1 review-fix）

- **前序 Codex/ZCode 阶段已结束**：P0-1/P0-2/P0-3 的提交与既有验证记录保留；不再描述旧会话仍在收尾
- **P0-1 已完成（提交 1e1838b feat: enumerate upstream release tags）**：GET /v1/releases/tags（git ls-remote --tags，复用 UpdateSpec.source 校验，timeout 默认 120s）、TagListResponse 协议类型、parse_ls_remote_tags 纯函数+单测（去重/剥 ^{}/倒序）、Tauri 白名单+测试、UpdatesView「上游标签」面板（按钮+下拉+已选显示）、i18n 中英。端到端实测：真实枚举 deepseek-harness 全部 tag（dsh-v0.1.3-alpha.1 最新在前）
- **P0-2 已完成（提交 ec905f3 feat: bound release slots with explicit removal）**：ReleaseStore 加 max_slots（默认3，`with_max_slots`），config.json 新增 `releases.max_slots` section（1-32 校验，agent 启动时读取）；register/register_prepared 满槽返回 ResourceBusy→HTTP 409 `release_slots_full`（消息列出已有槽位）；新 `remove(id)` + `ReleaseAction::Remove`（quiescent 守卫，current/LKG 拒绝→`release_slot_protected` 409）；data_error_response 补 ResourceBusy→CONFLICT 映射；UI 槽位行加释放按钮（current/LKG 隐藏，标记"当前使用/上次可用"）。core 新增 2 个行为测试。端到端实测全过（a:201 b:201 c:409 满，promote 后 rm-current:409 rm-free:200）
- **P0-3 已完成（提交 feat: switch release tag with install and promotion）**：UpdateAction::Switch + UpdateCommand.tag；`UpdateExecutor::switch_tag(tag)`——校验 tag→持 executor gate 持久化 config.ref_name=tag→已装同 version 槽位直 promote（秒切快速路径）→未装则走 install_owned（clone --branch tag→构建→register_prepared）→成功后自动 promote；handler 层 quiescent 守卫（Harness 运行中拒绝 release_change_conflict）；UI 已选 tag 显示"切换到此标签"主按钮。端到端实测：register version=dsh-v0.1.2-rc.1 → switch → current=harness-x，config ref 已更新。nexus-cli UpdateCommand 字段同步
- **P0-4a runtime-discovery review-fix 当前接管**：隔离分支 `codex/nexus-takeover/runtime-fix1`（基于 `fc0c5d1`）保留前序只读 GET `/v1/runtime`，并完善 system 优先、`<data-root>/runtimes` 固定浅层 portable 候选、绝对 path/source/version 和 unavailable reason；git 保持 PATH-only
- **P0-4a 安全边界已补齐**：PATH/PATHEXT 受限且候选有上限；Corepack canonical/shebang/script 检测 fail-closed，Corepack 不执行并关闭 network/download prompt/default latest/auto pin/project spec；探测 cwd 拒绝祖先 manifest；cmd/bat 采用绝对系统 command processor、拒绝 shell 元字符并隐藏控制台；输出/轮次/子进程/kill+wait 清理有界；数据根与 runtimes 根 reparse point 拒绝，portable canonical 候选必须留在 runtimes 根内
- **P0-1 共享接线缺口已修复**：`nexus-launcher-core` Agent allowlist 补 `/v1/runtime` 与既有 `/v1/releases/tags`，两者只允许无 body GET；POST 和非空 GET body 拒绝；Tauri runtime 移除重复 validator，复用共享 gate
- **P0-4a 验证已更新**：runtime 定向 20 项、Agent 82、launcher-core 11、protocol 11、Tauri 7 tests 均通过；Tauri 资源由本工作树 `CARGO_NET_OFFLINE=true` 准备脚本生成且被忽略。当前主机只有 Windows target，Unix Corepack/symlink 条件测试代码已添加但未在本机编译/实跑，Windows junction escape fixture 也未实跑
- **当前阶段报告**：`artifacts/takeover/runtime-discovery-report.md`；本轮未启动真实 Harness、未安装/下载、未改 PATH 或用户配置
- **P0-4b/c 与 UI 仍待做**：安装/下载、Corepack 显式配置、运行时注入和界面不属于本阶段
- **Corepack 官方事实**：官方 `https://github.com/nodejs/corepack` 说明仅 Node `>=14.19` 且 `<25` 随 Node 附带 Corepack；不能假设所有 Node 版本自带 Corepack 或 pnpm 调用不会下载
- 坑：i18n.ts 是 en+zh 两个同 key 对象，勿用全文件 key 去重；本机有钩子会把 	 转义还原成真实 TAB，Rust 里避免 char 转义字面量，用 split_whitespace 类方案
- GUI 截图验收按协议合批：批1（P0-1/2/3/4/8）完成后统一实测
- 历史状态：最新提交原为 683fe8f（34 提交）；nexus-agent doc-test 曾因 target 缓存旧 rlib 报 E0463，重跑即好

- 架构基线：`docs/architecture-baseline.md`（Phase 14；注意其中"默认 127.0.0.1:3090"的表述将随待办15改为 OS 分配，开工时同步修订文档）；阶段报告在 `artifacts/`

## 已知关键实现坐标

- `UpdateSpec.ref_name` 单一字段（默认 "main"）：`crates/nexus-core/src/lib.rs:3863` → tag 枚举要替换的正是这里的产品形态
- release 槽位：`releases/<id>/manifest.json` + `release-pointers.json`（current/last-known-good 原子指针）
- checkpoint：`checkpoints/` + 两阶段 intent journal（`run/`），恢复仅元数据（待升级为配置文件快照）
- 更新执行器：clone→可选 build/verify→原子发布槽位，任务持久化于 `update-state.json`
- 就绪探针：loopback 纯 HTTP(2xx) 或 `tcp://`（防 SSRF；官方 DSH 根页无 token 返回 401，故 tcp 探针）
- Profile 渲染：`HarnessLaunchSpec.args` 中 `{profile}` / `{release}` / `{release_root}` 占位符
- **DSH CLI 定位器（待建，多任务共用）**：插件管理/快照恢复物化/终端都要调官方 dsh 命令，参照 desktop-cli.ts 的 RunAsNode 引导，做统一的"运行时+安装方式 → dsh 命令"解析器
