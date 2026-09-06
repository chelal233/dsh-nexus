updated: 2026-09-06 by ZCode (运行时工具获取契约反转：内置运行时随包分发，下载链路退役)

# dsh-nexus 项目状态与需求基线

## 内置运行时批次①已落地（2026-09-06，ZCode）：bundled 观测档 + 下载链路停用

- **解析顺序生效**：pin → system → nexus（遗留 runtimes/ 缓存只复用不下载）→ **bundled（新，安装目录 `<exe dir>/runtime`）** → `ConfigureExternal`（无可用品时计划动作，替代原 ProvisionPortable/InstallSystem 的发射；旧枚举值保留线兼容但不再产出）
- **bundled 布局契约**：`<exe dir>/runtime/node/node.exe`（官方 zip 根布局）+ `<exe dir>/runtime/pnpm/pnpm.cjs`（standalone 入口，经 bundled node 直启，**不经过 corepack**）。git 无 bundled 档——git 兜底=内嵌 libgit2 worker（既有）
- **探测实现**（crates/nexus-agent/src/runtime.rs）：`bundled_candidates` 枚举（node 直探 `--version`；pnpm 由 bundled node 前缀执行 `pnpm.cjs --version`，与生产 node_script_argument 同一 verbatim 边界语义）；观测 source 字符串 `"bundled"`；`canonical_configured_path` Bundled 归类 System（canonicalize+拒远程盘）
- **协议**（crates/nexus-protocol）：`RuntimeOwnership::Bundled`（serde "bundled"）；`RuntimePlanTool.warning` 可选字段（`bundled_pnpm_major_skew`）
- **pnpm 策略 A 落地**（runtime_plan.rs）：exact 匹配优先；bundled 且同 major → Reusable + `warning=bundled_pnpm_major_skew` + 动作理由 `compatible_bundled_pnpm_major_skew`；跨 major → Incompatible → `ConfigureExternal`。核心新函数 `package_manager_same_major`（runtime_requirements.rs）
- **cold 收尾**（cold.rs）：计划含 ConfigureExternal → 直接 settle 失败，错误信息列出各工具 reason 并引导「设置中指定路径或重装 Nexus 使用内置运行时」；supply 下载分支保留但已不可达（先停用后删除）
- **传播零改动**：`runtime_from_plan` 把 plan 工具 path+ownership 转 pin，bundled 自动随 pins/env/command 单一漏斗进入 install/build/start/物化
- **验证**：cargo check --offline workspace 干净；core+protocol 48、agent 单线程 148/148、supply 测试过；doc-tests 过。并行全量下 `timeout_ends_owned_descendant_before_returning`/`timed_out_probe_reports_a_reaped_child` 偶发失败为**既有互杀抖动**（原始 main 504fb70 复现，单线程全绿），与本次无关
- **待做（批次②）**：WiX/构建脚本把 node zip + pnpm.cjs 实际打进安装目录 runtime/；批次③ UI 来源显示「内置」+ 偏斜警告 + 收起下载设置；批次④ registry 镜像回退与依赖安装实时进度；清理批物理删除下载代码

## 内置运行时批次②③④已落地（2026-09-06，ZCode，提交于 15ad6e7 之后）

- **批次② 打包**（apps/nexus-launcher/src-tauri/scripts/prepare-runtime.mjs，新）：构建时取 node v24.20.0 官方 win-x64 node.exe（SHASUMS256 校验）+ pnpm 11.7.0 完整 npm 包树（registry dist.integrity sha512 校验），布局=`resources/runtime/node/node.exe` + `resources/runtime/pnpm/`（入口 `pnpm/bin/pnpm.cjs`，bin shims→dist/pnpm.mjs 相对引用自洽；pnpm 11 要求 Node ≥22.13，bundled 24.20 满足）。冒烟实测：bundled node 直跑 bundled pnpm 输出 11.7.0 ✓。幂等（manifest 比对，命中跳网络）；缓存于 target/bundled-runtime-cache/。镜像变量 NEXUS_NODE_DIST_MIRROR / NEXUS_NPM_REGISTRY（受限网络用 npmmirror）。tauri.conf resources 增 `"resources/runtime/**": "runtime/"` → 安装目录 runtime/（正是 bundled_runtime_dir 观测位）；beforeBuildCommand 接入 prepare:runtime；WebView2 维持 embedBootstrapper（用户拍板不随包离线安装器）
- **批次③ UI**（App.tsx/i18n.ts）：工作台工具行显示版本+来源徽标（系统来源/Nexus 来源/内置，复用 runtimeToolSourceLabel，类型加 bundled）；缺失文案「按所选版本补齐」→「不可用」；「下载偏好」→「依赖安装源」单选（official/npmmirror，仅管依赖下载 registry），安装模式下拉移除（mode 值保留透传）；设置页同语义改造 + pin 帮助文案更新为「系统优先其次内置」；冷切换进度块新增策略 A 警告行（`warning` 含 bundled_pnpm_major_skew 时显示人话说明，中英）
- **策略 A 警告通道**：ColdOperation 新增持久化 `warning` 字段（protocol）；cold 快路径把 plan 工具 warning 拼入并经 record_foundation_plan 落盘；前端展示后安装完成随终态块消失
- **批次④（半）**：install 失败且 source=official 时错误追加「切换依赖安装源到 npmmirror」指引（dependency_registry_hint），前端 localizeBackendError 加签名 → 人话+行动指引（中英）
- **验证**：cargo check --workspace --all-targets 0 error；agent 单线程 148/148（cold 12/12）、core 36、protocol 13、supply 19；前端 tsc+vite build 过、37 tests pass、typecheck 过。**注意**：i18n 有既有同 key 双表结构，新增词条已成对添加
- **遗留验收**：完整 `tauri build` 打包验收**暂被阻塞**——本机有一个正在运行的 nexus-launcher-app（src-tauri/target/release，PID 30604，其 agent 19564、Harness 占 3080），锁定 target/release 二进制导致 cargo 重链接 os error 32；**不得杀用户活实例**。应用关闭后重跑 `pnpm tauri build` 验证 MSI/NSIS 含 runtime/ 即闭环
- **仍待做**：安装期 pnpm 输出实时流式进度（现仅诊断包留档+阶段百分比）；清理批物理删除下载死代码（runtime-supply 的 PublishPortable/InstallSystem 路径与测试、HttpDownloadClient、SourcePolicy）


## 运行时契约反转（2026-09-06 用户拍板）：内置 node/pnpm/corepack，下载链路退役

**产品契约**：解决小白用户干净机器装好 Nexus 却因缺环境/网络断点跑不起来的问题。工具获取从「系统→下载」双轨改为「用户自带 → 内置」双轨：

- **解析顺序（node/pnpm/git 三工具同一链，各自独立）**：① 用户显式指定路径（pin，最高优先；多版本共存时用户自己挑，UI 已有「手动指定运行时路径」输入框 + `set_runtime` pin 通道承载）→ ② 系统发现可用（满足该上游 release 的版本要求才算可用）→ ③ Nexus 内置（安装目录随包分发）→ ④ 都不满足 → 如实报错引导指定路径，**没有第四步下载**
- **pnpm/node/corepack 都随 Nexus 分发，不独立操作**；git 内置回退已存在（bc00e15），三者统一为同一契约
- **pnpm 精确版本偏斜策略（用户选 A）**：内置 pnpm 版本跟随「打包时当前上游 packageManager 要求」（首发 11.7.0），每次 Nexus 发新版刷新；运行时遇不匹配 → 同一 major 内放行 + 界面可见警告，跨 major 如实失败并引导用户指定自己的 pnpm 路径。后续若需多版本随包，直接往安装包里塞（现在不需要）
- **Node 无偏斜问题**（上游只有 engines 范围要求）：内置打包时点的一个 LTS，随 Nexus 发版刷新
- **WebView2（2026-09-06 定）**：不随包离线安装器；维持 Tauri WiX 默认在线引导器（缺失时安装阶段自动装）+ 应用启动检测缺失给指引兜底。理由：Win11 必有；Win10 多数有（Windows Update 推送）；LTSC/Server/ghost 系统可能没有，由引导器覆盖
- **存放位置**：内置运行时放**安装目录**（随 Nexus 换装整体升级）；下载的兼容性运行时概念取消，数据根 `runtimes/` 仅保留历史物
- **既有代码影响**：`nexus-runtime-supply` 的下载/发布机器（PublishPortable、InstallSystem、Node zip/npm tarball 校验下载）整体死代码——**先停用后删除**（先在计划解析停用 + UI 收起入口，物理删除放清理批）。设置页「工具下载源 official/npmmirror」作废；**pnpm registry 镜像设置保留**（管上游依赖安装走哪个 registry，与工具下载是两回事，是首装超时的解药之一）。首装引导工作台「安装运行时」阶段消失
- **不变红线**：不改系统 PATH、全绝对路径调用、能复用不下载原则（现在进一步=永不下载工具）；解析变化发生在 pins/env/command 单一漏斗，冷安装/启动/快照物化/终端自动跟随
- **实施批次**：① supply 侧 ReuseBundled 档 + 停用下载 → ② WiX 打包 node/pnpm/corepack → ③ UI 来源展示与下载设置收起 → ④ registry 镜像回退 + 首装依赖安装实时进度 → 清理批删死代码。待办基线 P0-4 中「按需下载/便携与系统安装模式」的描述自本节起被本契约取代


## P0 缺陷修复第二轮：占位符双路径（2026-09-05）

- 首次 retarget 修复引入回归：替换 `releases\<old>` 时保留了值的前缀（`Nexus\`），渲染后变成 `Nexus\C:\...
eleases\...` 双路径 → Harness 启动 os error 267（目录名称无效）
- **修正**：retarget 改为把值中**从开头到槽位段结束**的整个前缀替换为 `{release_root}`（值以占位符开头）；用户 config.json 的 args[0] 已二次修复为 `{release_root}pps/cli/lib/bin.js`
- 教训：Windows 文本模式 python 写入会把 LF 转 CRLF，改 Rust 源码后需归一化行尾（本次已 amend）；`{release_root}` 是子串替换语义，占位符应位于值的开头

## P0 反馈第十轮（2026-09-05）

ActionButton 全局补 `type="button"`：此前所有 ActionButton 在 `<form>` 内默认为 submit，「添加参数/移除参数」点击会触发表单提交（表现为变成保存/取消）。submit 专用按钮本就是原生 `<button type="submit">`，不受影响。

## [进行中] 自动化工作实例：冷切换 dsh-v0.1.3-alpha.1 验证中

## 深度 review（2026-09-06，四线并行审查）与修复

四条审查线（路径/占位符、配置写入+进程生命周期、状态机+端点、前端表单+i18n）确认了 12+ 个问题。已修复（提交 fix: enforce launch placeholder invariant and harden retarget）：

1. **占位符不变量下沉到 core**：`ReleaseStore::{promote,rollback,restore_checkpoint_release}` 全部在发布指针前调用新增的 `retarget_launch_placeholder`（config transaction 原子改写）——此前只有 switch_tag 一条路径有 retarget，cold promote/ReleaseAction::Promote/Rollback/checkpoint restore 都会把启动配置留在旧槽位（与实测钉死 bug 同类）
2. **cold 发布改用 `{release_root}` 占位符**（原 cold.rs:813-833 写死具体槽位路径，是新配置钉死问题的源头）
3. **retarget 加固**：大小写不敏感（Windows 路径）+ 边界校验（rc1 不匹配 rc10）+ 改用 ConfigStore::transaction（原 load-modify-write 会覆盖用户并发保存）；updater 里的重复实现删除，收敛到 core 单点
4. **前端 kv 编辑器**：未填完的参数行保存时明确报错（此前产生裸 `--` token 进 config，实测踩过）；脱敏参数分支不再把 [REDACTED] 字面量发给后端；readiness URL 脱敏时 preserve 标志修正（此前永远不生效，保存会静默清空已存 URL）

审查确认但延后到 P1 的：
- **agent 无构建版本握手**：launcher 复用旧 agent 进程不校验二进制新旧（实测踩坑根源），需 HealthResponse 加版本 + AgentRuntime 比对
- **Harness spawn 未用 Job Object**：stop/kill 只杀直接子进程，node 孙子进程（插件/esbuild）泄漏占端口；runtime-supply 已有 Job Object 设施未接到 supervisor
- **Windows graceful stop 永不优雅**：无信号投递，必耗满 5s 后 kill；且 stop 持 lifecycle 锁 5s 阻塞全部 GET 端点
- **failed 无快循环**：崩溃后 UI 最长 8s 才显示失败
- recovery 期间（无 PID）stop 被拒的死锁窗口；retarget 不覆盖 runtimes 目录的 node pin 变化

## P0 状态机核查结论（2026-09-06）

- 实测显式 stop：终态 = stopped（"graceful stop 超时后强杀"，Windows 下 Harness 不响应优雅停止，exit 1 如实记录）——**此前怀疑的 stop→failed bug 不存在**，当时是 Harness 已自行崩溃（插件树损坏），stop 返回既有 failed 状态。撤回该 bug 报告
- /v1/harness（8s 轮询快照）与 /v1/harness/ui（fail-closed 实时双同步）的不一致 = 崩溃窗口内的轮询时差（≤8s），非状态源缺陷；认证面板的 Failed 是真相
- 启动中/停止中的 i18n 与状态机均正确；用户观察到的错误状态源自插件树崩溃窗口
- 当前唯一阻断不变：desktop 配置档插件树损坏 → 用户走快照恢复（rc.1 时期健康快照）即愈

## P0 链接农场重建实验（2026-09-06，自动化第四轮）

- 实验：删除 `profiles/node_modules` 链接农场 → 启动 rc.1 → **农场被重建且 @deepseek-ai/dsh-settings 版本 = 0.1.3-alpha.1**（npm 上不存在此版本，来自本地 alpha.1 槽位的 workspace 链接）
- 同轮删除农场后首次启动：导出错误全部消失（只剩 file-upload 重复——快照恢复把清单里的 file-upload 带回，已再次从清单移除）；随后启动：农场重建（alpha.1 包）→ 导出错误复现
- **结论**：农场每次启动重建，但 @deepseek-ai/* 解析到了 alpha.1 槽位的包（rc.1 启动却链接 alpha.1 的包！）——物化器的包来源存在持久化的陈旧引用，或解析顺序问题。需要读上游 app-boot 源码（profile.ts 的 symlink 维护循环 + index.js 的闭包发现）确定：1) 闭包发现的包来源路径；2) 为什么 rc.1 启动会链接 alpha.1 槽位的包
- **当前机器状态**：release=alpha.5 槽位→已切 rc.1（指针 rc.1，dsh-file-upload 已从清单移除，manifest 干净）；启动仍失败（导出缺失，因农场=alpha.1 包）；`.nexus-backup-20260906/` 与 `node_modules.stale-alpha1` 保留；plugin-desktop\node_modules 的 0.1.2-alpha.1 三件套验证过满足全部导出（应急可 junction，但 loader 不走 profiles/node_modules 解析 @deepseek-ai/*——loader 内部映射优先，junction 方案无效已证实）
- **已知可行组合**：desktop 自带 0.1.2-alpha.1 harness + 其捆绑官方包曾正常运行数周（用户原生态）；本地三个 GitHub tag 槽位均与 rc.2 时代插件存在导出面不匹配

## P1 决策记录（2026-09-06，自动化第十一轮）

- **CTRL_BREAK 优雅停止方案已实施又回退**（revert: ctrl break delivery）：实施后测试运行器被控制台事件误杀（0xC000013A，PID 组复用可能命中无辜进程），且 Node 默认不处理 CTRL_BREAK（无真实优雅收益，行为与 TerminateProcess 等价）。最终停止路径 = 5s 宽限窗（对有 handler 的进程有效）+ Job Tree 终止兜底。**结论：Windows 下 node 类负载没有真正的优雅停止，5s 等待是合理成本**，不再尝试信号方案

## P1 进展（2026-09-06，自动化第十三轮）

- **端口去固定化完成**（feat: ephemeral agent port with discovery record and probe fallback）：
  1. Agent 默认端口 = OS 分配（`--port 0`），实际端口+实例身份+数据根身份+PID 发布到 `run/agent.json` 发现记录（原子写）；显式指定端口（`--port N`/NEXUS_AGENT_PORT）照旧钉死
  2. launcher-core probe：配置端口传输失败时回退读发现记录（校验 data_root_id 拒绝陈旧/异源记录）→ 发现端口探测
  3. launcher 启动子 agent 默认传 `--port 0`（配置端口非默认值时透传）
  4. E2E 实测：`--port 0` → 发现记录 49322 → 该端口 health ok + 实例身份匹配 ✓
- 兼容性说明：NEXUS_AGENT_PORT/--port 显式钉死的部署照旧；nexusctl 旧用法（显式 --port）不受影响

## P1 进展（2026-09-06，自动化第十八轮）

- **桌面通知完成**（feat: desktop notifications for harness failures with settings toggle）：插件权限（capabilities notification:default）、src/notifications.ts（权限申请+偏好 localStorage+best-effort 发送）、Harness 崩溃 transition 通知（per-run 去重）、设置页启用开关（默认开）。设置页静态"Available through Tauri"行替换为真实开关

## P1 进展（2026-09-06，自动化第十八轮）

- **Profile 新建完成**（feat: create profiles from the shipped web template）：ProfileAction::Create + ProfileStore::create（core）——校验名称 → staging 目录写三件套（package.json 模板 dsh-profile-<name>/base+web-app bundles/live patch、cordis.patch.yml、pnpm-workspace.yaml）→ 原子 rename 发布 → catalog 登记；已存在目录/名称 fail closed。Agent `profile_create` 端点（dsh home 不可用时明确报错）。UI 配置档页「新建 Profile」名称输入+按钮。core 回归测试（模板文件断言+重复失败）。nexusctl 字段/分支补齐

## P1 进展（2026-09-06，自动化第十七轮）

- 崩溃自动留证的测试干扰已修（4cd0249）：cfg(not(test)) 跳过自动捕获；清理 unused 警告。130 测试全绿。工作协议补充：新增 AppState 字段必须同步全部测试构造器（本次 5 处）；带副作用的异步行为（诊断/快照写盘）在测试构建统一关闭

## P1 进展（2026-09-06，自动化第十四轮）

- **配置文件快速查看/打开完成**（feat: quick-open profile files and directories from the profiles page）：ProfileAction::OpenPath + ProfileOpenPathResponse（协议）；agent `profile_open_path` 端点——四种有界目标（settings/profile_dir/profile_patch/plugin_manifest），路径仅由 DSH home+profile 派生，explorer/start 打开（CREATE_NO_WINDOW）；配置档页四按钮 + i18n。nexusctl ProfileCommand 字段补齐 + OpenPath 分支覆盖

## P1 进展（2026-09-06，自动化第十二轮）

- **错误人话化映射落地**（feat: plain-language error mappings）：localizeBackendError 新增 11 类特征签名 → 人话+行动指引（插件树失败→恢复模式、导出不匹配→恢复快照、duplicate entry→移除旧副本、EISDIR/267→重建配置档、槽位满/受保护、快照版本未安装、连接拒绝/超时/拒绝访问）。中英双语，未命中签名回退后端原文

## P1 进展（2026-09-06，自动化第十轮）

- **Job Object 已接入 Harness spawn**（提交 fix: assign harness process tree to a kill-on-close job object）：spawn 时创建 kill-on-close Job 并分配整棵进程树（dsh.rs 新增 assign_process_to_job 按 HANDLE 泛化）；stop 超时后 TerminateJobObject 终止整棵树（此前只杀直接子进程，插件/esbuild 孙子进程泄漏占端口）；Agent 进程退出时句柄关闭 → 整树死亡（"Agent 随 Launcher 退出"语义完整）。测试构建（cfg not(test)）跳过 Job 以避免并行测试互杀，生产行为完整

## P1 进展（2026-09-06，自动化第九轮）

- **agent 二进制新鲜度握手已实现**：HealthResponse 新增 `binary_path`（agent 报告 current_exe）；launcher-core 的 `AgentRuntime::start` 在采用既有 agent 前比对 binary_path 与自身解析的 agent 路径（规范化+canonicalize 双重比较），不一致 → 优雅 stop 旧实例 → 正常 spawn 新二进制；无 binary_path 的旧版 agent 向后兼容直接采用。实测踩坑的"launcher 更新后旧 agent 被复用"从此根治
- **failed 快循环**：Harness 崩溃后 UI 轮询从 8s 收紧到 400ms，失败状态 ≤1s 内可见
- P1 队列剩余：Job Object（进程树清理）、优雅停止信号+stop 持锁重构、Profile 新建 UI、配置文件快速查看、DSH 终端、通知、首次运行向导、崩溃自动留证、错误映射、端口去固定化

## P0 node_modules 清空实验（2026-09-06，自动化第八轮）与晨报

- 实验：备份后清空 desktop/node_modules → 启动 → harness 报 "cannot resolve profile bundle dsh-auto-review ... run dsh plugin install"——**harness 启动不会自动重装插件**，需要显式 dsh plugin install（21 个插件的全量网络安装）。已回滚：node_modules 从备份完整恢复（dsh-settings-file ✓）
- **晨报总结（用户醒来先读这段）**：
  1. 机器当前状态：release 指针 = rc.1；配置档 node_modules = Sep 2 desktop 时代（与 rc.1/alpha.1/alpha.5 槽位均存在导出面不匹配）；桌面 fallback 链接与备份完整
  2. 今日已修复的 Nexus 真 bug：配置钉死占位符化（4 条路径全覆盖）、verbatim 路径、cold 发布占位符、retarget 加固+事务化、kv 编辑器 4 项、ActionButton submit、健康快照直接恢复（restore 接受快照 id，已实战验证合成 checkpoint + 两阶段恢复 + 指针回滚）、EISDIR verbatim
  3. **剩余阻断的本质**：21 插件配置档（market 多代混装）与任何 GitHub tag 槽位都存在导出面错配；桌面捆绑 0.1.2-alpha.1 官方包是唯一全满足的副本（fallback 链接已恢复指向它）
  4. **恢复可用的两条路（用户拍板）**：A. 重装插件集——`dsh plugin install` 全量重装（网络下载 21 包，装完与 rc.1/alpha.1 自洽）；B. 继续用桌面应用跑（现状可用），Nexus 等待插件生态版本对齐后再接管
  5. 代码侧无剩余已知 bug；自动化已删除条件未到（P0 尾巴只剩 A/B 决策后的收尾）

## P0 INSTALL_ANCHOR 定位（2026-09-06，自动化第七轮）

- **heal 锚点确认**：`profile-boot` 中 `INSTALL_ANCHOR = fileURLToPath(new URL("../package.json", import.meta.url))`——锚点=运行中 harness 自己的 apps/cli/package.json，heal 理论上永远治愈到当前运行槽位
- **矛盾未解**：实测 rc.1 启动后共享 fallback 仍指向 alpha.1 槽位（0.1.3-alpha.1 版本）——heal 应在 composeProfile 时运行，但农场内容未跟随。需下一轮：插桩观察 rc.1 启动时 composeProfile/heal 是否执行（或读启动日志/调试输出）
- **绕开时序的 Nexus 侧方案（下一轮实施）**：heal 语义已知=对当前槽位 workspace（vendor/*、packages/*/*，见槽位 package.json workspaces 字段）的每个包，在 `~/.dsh/profiles/node_modules/<pkg>` 建/校 symlink（Windows junction）→ Nexus 在启动 Harness 前自行执行同样的 heal（不依赖上游时序），并在 switch/restore 时清理旧农场。这正是此前设计的"Nexus 侧 heal"
- 实测参考：rc.1 槽位 workspace 结构已确认（vendor/*、packages/*/*、native/landlock-run）；desktop 捆绑 0.1.2-alpha.1 副本满足全部插件导出（已验证）；alpha.5 内部模块满足除 settingsNamespace 外全部导出
- 插件兼容矩阵速查：settings-file 需 deepEqualJson（rc.1+ ✓）；session-persistence-jsonl 需 DEFAULT_PREPARED（alpha.5+ ✓）；sandbox/llm-deepseek 需 assertNever/CallId（alpha.5+ ✓）；reasoning-effort/better-sidebar 需 settingsNamespace（仅 desktop 捆绑 0.1.2-alpha.1 与 rc.2+ 有）→ **当前 21 插件清单与任何本地槽位都无法全量兼容，需要逐插件取舍**（卸载旧 API 插件或换槽位），这是用户级决策不是代码 bug

## P0 heal 函数定位（2026-09-06，自动化第六轮）

- **机制完全定位**：两条线的 app-boot 均含 `healProfilesModuleFallback`（profile.ts，两份源码长度一致 37889 字节）：每次启动创建 `~/.dsh/profiles/node_modules` 共享 fallback 并把链接**重新治愈到当前安装（installAnchor）**的包；`moduleFallbackCurrent` 校验不匹配才重建；`ensureProfileSymlink` 对已存在的链接直接跳过（粘性）；"cleanup removes only dsh-owned links"
- **待解的最后问题**：rc.1 启动后共享 fallback 链接仍指向 alpha.1 槽位——要么 heal 未运行（启动崩溃时序在 heal 之前？），要么 installAnchor 解析到了 alpha.1（持久化锚点？）。下一轮：grep app-boot 源码中 healProfilesModuleFallback 的调用点与 installAnchor 的来源（从运行进程路径还是从持久化状态），即可锁定修复点
- Nexus 侧修复预案（锁定后实施）：switch/restore/启动 Harness 前调用同一 heal 语义（或直接删除共享 fallback 让 harness 自己 heal 到当前槽位——需确认 heal 在崩溃前运行的时序）

## P0 双布局发现（2026-09-06，自动化第五轮）

- **上游两条发布线使用不同的模块布局**：alpha 线（0.1.3-alpha.1）的 app-boot 维护 **profiles 级共享链接农场**（`~/.dsh/profiles/node_modules` → 当前槽位的 vendor/packages）；rc 线（0.1.2-rc.1）的 app-boot 维护 **profile 内 fallback**（`.dsh-module-fallback` + 投影链接）。两套布局互不认识
- **当前病灶**：alpha.1 启动时在 profiles 级留下的共享农场（全部链接指向 alpha.1 槽位）残留；rc.1 启动只维护自己的 fallback，不清理/更新父级农场 → 插件解析时命中父级农场里的 alpha.1 版本包（缺 settingsNamespace/deepEqualJson）→ 崩溃。删掉农场后 rc.1 启动会**重建农场但内容仍是 alpha.1 版本**（重建来源待查——疑为 alpha.1 boot 写入的持久化引用或 pnpm 缓存）
- dsh-file-upload 重复已随快照恢复回来并再次从清单移除（当前 manifest 干净）
- export 错误清单（rc.1 boot + alpha.1 农场）：settings-file 需 deepEqualJson、session-persistence-jsonl 需 DEFAULT_PREPARED、sandbox 需 assertNever、llm-deepseek 需 CallId、reasoning-effort/better-sidebar 需 settingsNamespace
- **下一轮任务**：1) diff rc.1 与 alpha.1 的 app-boot 源码（packages/boot/app-boot/src/profile.ts），确定 profiles 级农场的创建者与重建数据源；2) 找到重建来源后修复（Nexus switch/restore 时清理或改写）；3) 若无法从源头修，rc.1 启动前阻止农场重建的实验性方案待评估

## P0 链接农场机制实锤（2026-09-06，自动化第三轮）

- **机制实锤**：`~/.dsh/profiles/node_modules/` 是 harness 启动时自动创建的**链接农场**（junction/link，非真实目录）——全部指向"当前运行槽位"的 vendor/packages（插件借此解析官方依赖）。这就是上游"Profile module fallback"的落地形态
- **切换 bug 的完整机制**：alpha.1 启动把链接农场指向 alpha.1 槽位；**切回 rc.1 后链接农场未跟随更新**（仍指 alpha.1 槽位）→ 插件解析到 alpha.1 版本的官方包 → 导出缺失 → 插件树崩溃。恢复快照只回滚了声明式文件（package.json/lockfile/patch），链接农场不在快照内容里，也没有任何流程更新它
- **下一个自动化任务（精确定位）**：1) 在 `~/.dsh/` 下搜索引用 alpha.1 槽位路径（`harness-dsh-v0-1-3-alpha-1-1788631620419639700`）的文件（排除 node_modules 内部链接本体），找到记录"当前槽位路径"的指针文件；2) 确认 harness 物化器读取该指针的位置（槽位源码 `packages/boot/app-boot` 的 profile.ts / index.js 中搜索 node_modules 链接农场的创建逻辑与指针来源）；3) 修复方向：Nexus 在 switch/promote/restore 时同步更新该指针（或在启动 Harness 前重写链接农场指向当前槽位）；4) 修复后：删链接农场 → 启动 rc.1 → 验证农场重建指向 rc.1 且插件树加载成功
- 已备份：`~/.dsh/profiles/node_modules.stale-alpha1`（alpha.1 指向版本的完整副本）；`.nexus-backup-20260906/`（fallback 内容）
- 快照直接恢复功能已实现并验证：restore 快照 id → 合成 checkpoint → 指针回 rc.1 → 声明式文件回滚（dsh-file-upload 回到清单）。唯一剩余 = 链接农场指针更新

## P0 插件树兼容性终局分析（2026-09-06，自动化第二轮）

- 实测矩阵：desktop 捆绑 0.1.2-alpha.1 官方包**满足全部所需导出**（settingsNamespace/deepEqualJson/DEFAULT_PREPARED/assertNever/CallId 全 ✓）；rc.1 内部缺 settingsNamespace 等；alpha.1 内部几乎全缺；alpha.5 部分 ✓ 部分 ✗。**配置档里 9 月 2 日物化的插件副本（desktop 时代）与任何本地槽位都不匹配**
- **物化器粘性**：harness 启动物化器遵循 "existing pnpm entries win"——desktop 时代的旧副本永远不会被自愈覆盖，删掉杂散树后重物化仍装出错版本
- **确认设计缺口**：`checkpoint_restore` 只接受 checkpoint id；**3 个健康快照无法直接恢复**（UI 只有详情/检查），而 desktop 模型中健康槽位本身就是恢复点 → 需要实现"快照直接恢复"（复用两阶段 journal + 物化）
- **fallback 清理教训**：`.dsh-module-fallback` 与投影链接是上游 loader 的正当机制（原 fallback 指向桌面应用自带包，是插件能跑的功臣），不可当作污染清理（已从备份恢复原状）
- 用户恢复的完整临时路径：恢复快照功能实现后一键修复；临时手工方案=用桌面捆绑的 0.1.2-alpha.1 官方包副本 junction 替换配置档 node_modules 对应条目（junction 已建，实测 loader 不走 profiles/node_modules 解析 @deepseek-ai/*，而是 loader 内部映射到运行 harness 的内部模块 → junction 无效，故临时方案也不可行）
- **结论：必须实现快照直接恢复**，这是用户当前唯一出路（快照内容=desktop 时代自洽组合的备份）

## P0 解析链完整定位（2026-09-06 凌晨，自动化）

- **解析链实锤**：配置档插件（desktop/node_modules/...）import `@deepseek-ai/dsh-settings` → 命中 **`C:\Users\PC\.dsh\profiles\node_modules\@deepseek-ai\dsh-settings`**（profiles 级 pnpm 工作区共享 node_modules）——该共享树在 alpha.1 启动时被重新物化为 alpha.1 版本（0.1.3-alpha.1，无 settingsNamespace/deepEqualJson 导出）
- **切回 rc.1 不自愈的原因**：rc.1 启动在插件树加载阶段就崩溃，共享树的重新物化（harness boot 物化器 + lockfile）没有机会把版本拉回 rc.1；且 profiles 级 pnpm-lock.yaml 已被 alpha.1 启动改写，pnpm install 会按 alpha.1 锁定文件继续装错版本
- **验证手段已留**：`%TEMP%\resolve-test.cjs` 可随时查插件上下文解析到的 dsh-settings 版本
- **恢复方案（下一轮自动化执行）**：1) GET /v1/checkpoints {action:"detail", id:"snapshot-1788614094193"} 取 rc.1 时期健康快照的文件内容（应含 profiles 级或 profile 级 pnpm-lock.yaml/package.json）；2) 将对应锁文件写回 `~/.dsh/profiles/`（先备份现值）；3) 用便携 pnpm 在 profiles 目录跑 install 重新物化；4) 启动 Harness 验证；5) 若快照不含 profiles 级锁文件，改为检查 harness 物化器的触发条件并从槽位侧修复
- **新发现的设计缺口（P1 项）**：健康快照（3 个）在 UI 只有 详情/检查 没有直接恢复入口，恢复仅能通过 checkpoint——需确认健康快照是否可直接恢复，不能则补齐（desktop 模型里 3 个健康槽位本身就是恢复点）
- 今日冷切换实战验证通过：switch→confirm→install→build→promote 全链路 3 分钟，占位符/retarget 修复实战有效

## P0 启动失败最终定性（2026-09-06，版本偏斜）

- 实锤：槽位内无 dsh-settings/dsh-llm 独立副本（loader 将 @deepseek-ai/* 映射到 harness 内部模块）；配置档插件是 rc.2 时代（dsh-settings-file@0.1.1-rc.2），要求 settingsNamespace/deepEqualJson 等新导出；rc.1/alpha.5 两槽位都太旧 → **切哪个槽位都会插件树失败，这是版本偏斜不是污染**
- fallback 链接指向原 dsh-desktop 安装目录（D:\dsh-local\dsh-desktop\...）——配置档插件运行一直隐性依赖桌面应用自带的官方包，这就是"脱离 desktop"必须解决的架构依赖
- 我的 fallback 清理是误判（已从备份完整恢复，链接由 loader 自动重建）
- **下一步行动（自动化执行）**：冷切换到最新 tag（tag 枚举显示 dsh-v0.1.3-alpha.1 最新，晚于插件的 rc.2 时代）→ 验证插件树加载 → 成功则 P0 闭环
- 深层架构项（P1 首项）：插件官方依赖解析需要"跟随 harness 版本的官方包供给"——候选方案=把 @deepseek-ai 家族的 fallback 投影纳入 Nexus 管理（按版本分目录物化），而不是依赖桌面应用目录；需先研究上游 loader 的映射协议

## P0 验证进展与遗留（2026-09-05 晚）

- **EISDIR 已修复验证**：verbatim 路径修复后，rc.1 loader 正常引导（run 25/26 的 node 启动阶段已过）
- **当前阻断（非 Nexus bug）**：desktop 配置档插件树被 alpha.5 失败启动半迁移——`@deepseek-ai/dsh-settings` 从配置档消失、三个包的软链接（dsh-client-store/ui-primitives/ui-slots）被改写指向 `.dsh-module-fallback` 的跨版本副本 → rc.1 loader 报缺 `settingsNamespace` 导出
- **恢复路径（已告知用户走 UI 验收）**：配置档 → 检查点/快照清单 → 恢复 rc.1 时期健康快照（snapshot-1788614094193 等）→ pnpm 物化重建 node_modules → 启动
- **已知状态机 bug（测试中发现，待修）**：显式 stop 后状态发布为 failed（应为 stopped）——stop 终止子进程的退出码 1 被监控任务抢先发布；以及 /v1/harness/ui 的状态源与 /v1/harness 不一致导致认证面板显示陈旧 Failed
- 根治项仍待拍板：切换版本成功后自动物化配置档依赖（可预防此类半迁移）

## 决策反转确认（2026-09-06，用户拍板）：Nexus 托管模块农场

- **推翻"P0 最终决策"中的保持原样路线**，正式走 **Nexus 托管模块农场**：Nexus 拥有 `~/.dsh/profiles/node_modules` 的管理权，每次启动 Harness 前（以及 switch/restore 时）将农场链接重指到**即将运行的槽位**的 workspace 包
- **理由（小白视角）**：保持原样路线下，跨版本切换后陈旧农场链接会让**新旧两个版本都起不来**（alpha.1 农场污染 rc.1 启动，实测整夜修复）——对小白等于版本切换是不可逆自爆。托管路线下机器永不锁死：不兼容的插件可见失败，恢复模式兜底
- **边界不变**：不修改上游代码；农场是上游 boot 自己也动态维护的运行时目录，desktop 亦有同类先例（dsh-plugin-desktop 维护自己的投影链接）
- **已实现部分**：heal_module_farm（提交 362a63c）——spawn 前重指 38 个链接到运行槽位，已验证生效
- **活跃调查项（run 46 反常）**：heal 重指后插件导出错误依旧，启动后实测农场 junction 仍指 alpha.1 槽位。假设：harness 启动时的 pnpm 物化器（dsh.js/cold.rs 的物化路径）在 Nexus heal 之后重建/重指了 junction（pnpm 工作区状态或 lockfile 残留 alpha.1 引用）。下一轮：1) 启动后立即检查 junction 目标与物化器执行顺序；2) 读物化器代码确定 profiles/node_modules 的写入方与版本来源；3) 修复 = 物化器重指 Nexus heal 的目标，或 heal 移到物化器之后
- 边界重申：不修改上游代码；此前"P1 移除官方依赖供给管理"的记录作废，该机制以"Nexus 托管农场"形态回归

## 接管边界确认（2026-09-06，用户确认）

- 用户开放"接管与 desktop 同类的机制"，边界=**不得修改上游代码**
- **Nexus 侧 heal（heal_module_farm）合规复查通过**：只维护 `~/.dsh/profiles/node_modules` 链接农场（上游 boot 自己也动态维护的运行时目录，设计上预期外部维护者），junction 指向即将启动的槽位；不碰槽位内容、不 patch vendor/loader、不改 app-boot
- 边界内清单（desktop 先例）：链接农场 junction 维护、.dsh-module-fallback 管理、配置档目录创建（initProfile 语义）、调用上游导出函数
- 边界外清单：修改槽位文件、fork/patch loader 解析、改 app-boot 源码
- 待解技术问题不变：run 46 显示农场已重指 rc.1 但插件导出错误依旧——需插桩一次启动，确认插件实际解析路径被什么遮蔽

## P0 最终决策（2026-09-06，用户拍板）：插件不维护、切换保持原样

- **产品契约（用户原话级）**：Nexus 只替用户完成其无法自行完成的修复（无法启动/恢复模式之类的修复），**不帮助用户维护插件**——否则每次上游发版都得核对用户插件配置，不可持续
- **切换契约**：切换上游分支/标签时**保持原样**——不强制插件与 harness 版本一一对应，但必须保证切换后"该有的插件都还在"（清单与已装插件不丢失不重装）
- **兼容性错配的处理方式**：不匹配的插件加载失败**可见暴露**（启动日志+错误映射人话提示），由用户经恢复模式自行卸载/更新，或切回旧版本
- **P0 据此关闭**：现有实现已满足契约（切换/恢复不触碰插件清单与 node_modules 插件内容；链接农场 heal 只重指官方包到运行槽位=上游 boot 同语义；无任何插件重装逻辑）。此前设想的"官方依赖供给版本化管理/分目录"**从 P1 移除**（用户明确不做插件维护）
- 用户验证方式：alpha.1 槽位 + 现有配置档 → 插件树加载失败为预期行为（rc.2 时代插件 vs alpha 线 harness 的固有错配，错误映射已人话化）→ 恢复模式可见可操作 → 切回 rc.1 恢复工作组合

## P0 缺陷修复第三轮：release_root verbatim 路径（2026-09-05）

- 用户切回 rc.1 后启动仍报 EISDIR lstat 'C:'（node 主模块解析失败）。手动复现 rc.1 bin.js 引导正常，排除入口文件问题
- 根因：`ReleaseStore::release_root` 的 canonicalize 返回 Windows verbatim 路径（反斜杠问号前缀形式）；`{release_root}` 渲染出的入口 = verbatim + 混合斜杠，node 模块解析器无法处理（run 11 正常是因为当时 config 是具体路径未走 canonicalize）
- 修复：release_root 返回前剥掉 verbatim/UNC 前缀（dunce 风格）；影响 spawn cwd、入口渲染、{release_root} 占位符全链路
- 待用户指认：「启动中/关闭中」过渡态显示错误的具体页面/元素（i18n 映射 Starting→启动中 存在，怀疑是轮询快照滞后或某特定面板的 state 来源）
- 上一轮遗留：切换后自动物化 profile 依赖仍待拍板

## P0 反馈第九轮（2026-09-05）

删除设置页「更新配置」面板（分支/引用由版本槽位决定，Git 程序由运行时设置决定，来源挪走）：来源 URL 改为「上游标签与冷切换」面板内的可编辑输入 + 「保存来源」按钮，保存走 set_update（完整 UpdateSpec payload，ref_name/git_program 等取当前 config 现值，避免 ref_name 缺省重置为 main）。

## P0 反馈第八轮（2026-09-05，direct 模式退役 + 运行时归位）

1. **运行时设置整块（含运行时状态面板）从更新页迁至设置页**，置于 Harness 配置面板之前；更新页切换标签 payload 改用 config 持久化的 source/mode
2. **Harness 配置极简化（node-only）**：删除 Launch mode 选择器（direct 选项退役）、program/entry/工作目录手动输入、HarnessDiscoveryPanel 自动识别整块；只保留 就绪检测 URL（可选）+超时+token 勾选 + **结构化附加参数**（key-value 行编辑，`--profile {profile}` 自动保证存在并跟随当前配置档；保存时 program 兜底=node pin/`node`，entry 兜底=`{release_root}/apps/cli/lib/bin.js`）
3. 后端协议未动（direct 兼容保留，UI 不再暴露）；harness-config.test.ts 纯函数测试全部保留通过
4. 教训：SettingsView 的 discovery 块删除后注意散落的 setSelectedCandidateId 引用；大段 JSX 替换后跑 tsc 定位重复闭括号

## P0 缺陷修复：切换后启动配置钉死旧槽位（2026-09-05）

- **用户报障**：切回 rc.1 后 Harness 仍报 alpha.5 的错（栈路径指向 alpha-5 槽位）。**根因**：冷切换安装时把 harness.program/args 物化为 alpha-5 具体路径写进 config.json；switch 只切 release 指针，不同步启动配置 → 启动的永远是旧槽位代码
- **修复（提交 fix: retarget harness config to release placeholder on switch）**：`UpdateExecutor::retarget_harness_config`——promote 后把 config 中指向旧槽位的 `releases\<old>`/`releases/<old>` 前缀改写为 `{release_root}` 占位符（supervisor 为子串替换，mid-path 可用，渲染时跟随 current 指针）。挂在 promote_for_switch 单一漏斗（快/慢路径都覆盖）。附单元测试
- **用户 config.json 已就地修复**（args[0] → `Nexus\{release_root}pps/cli/lib/bin.js`），下次启动 Harness 即生效，无需重启 Agent
- **待办提醒**：这类“profile 依赖与 harness 版本不匹配”的启动失败（rc.1↔alpha.5 切换后 node_modules 过期）仍需“切换后自动物化 profile 依赖”根治——用户拍板后实施

## P0 反馈第七轮（2026-09-05）

概览 Harness 卡片的启动日志尾随改为**二级弹窗**（新增通用 Modal 组件：遮罩+居中卡片+右上关闭，点击遮罩可关闭），不再内联拉伸页面。按钮文案「查看启动日志」。**待用户拍板**：冷切换成功且版本变化后自动对当前配置档执行 pnpm 物化（修复 alpha.5 类“配置档依赖与 harness 版本不匹配”启动失败；写入 .dsh profile 行为与已验收的恢复物化一致）——用户确认后实现，然后 P0 收束进 P1。

## P0 反馈第六轮（2026-09-05）

冷切换状态块渲染条件收紧：仅当操作进行中（operationId 存在且 phase 非终态：running/cancelling/awaiting_confirmation）或 update job running 时显示；**终态（succeeded/failed/cancelled）即消失，不再常驻**。失败信息由操作横幅与按钮恢复可用承担。

## P0 反馈第五轮（2026-09-05，归位整理）

1. RecoveryDiagnostics（启动恢复状态+日志尾随）**移出配置档**→并入诊断页底部；ProfilesView 不再渲染
2. 概览 Harness 卡片：state=failed 时出现「查看启动日志尾随」二级展开（RecoveryLogTail）——启动失败日志从概览直达，不再藏在配置档
3. 设置页「运行时状态」面板（RuntimeStatusPanel+controller）**整体迁至更新页**运行时设置面板内（分隔线下方，手动检查运行时按钮）
4. 概览删除「Agent 操作」面板：强制重启按钮移入 Agent 生命周期 Metric 卡片
5. 概览删除「Harness 控制」面板：启动/重启/停止按钮+PID/退出码/最近错误详情移入 Harness Metric 卡片（未运行→启动；运行→重启/停止；failed→重启），卡片内含失败日志二级展开
6. 新组件 RecoveryLogTail（日志尾随单独抽取）；HarnessControlPanel 组件已删除

## P0 反馈第四轮（2026-09-05）

1. 冷切换状态块**条件渲染**：仅当 operationId 存在或 updateState 非 idle 时出现（含确认计划时内联显示），空闲时整块消失
2. 标签按钮按本地槽位状态显示「切换到此标签」(已装) / 「拉取此标签」(未装)，判断=releases[].version===selectedTag
3. 版本槽位行内加「切换到此版本」(promote)——秒切主入口；「释放」仍只对非 current/LKG 显示
4. 「确认运行时供应计划」从独立面板改为内联在上游标签与冷切换面板内（待确认时显示在进度块之前）
5. 删除「检查点」导航项（配置档枢纽内已有，CheckpointsView 保留 embedded 用途）

## P0 反馈第三轮（2026-09-05，accordion）

配置档目录改为**手风琴树**：默认全部折叠；点击 profile 行（整行可点，带 ▸/▾ 指示）展开/收起其子区域（已保存检查点+快照清单+插件清单）；「查看/查看中」按钮与徽标删除，行内仅保留「选择」（切换当前 profile）；RecoveryDiagnostics 移出子区域、每页只渲染一次（它是全局启动恢复状态）；CheckpointsView 的全局块（healthy 捕获错误/待恢复面板）在 embedded 模式下隐藏，避免每个展开的 profile 重复出现。测试改为：折叠断言（▸ 存在、Saved checkpoints/Plugin inventory 不存在）+ 直接渲染 ProfilePlugins 验证清单真实性。

## P0 反馈第二轮（2026-09-05，提交 nest-profile-children）

1. **配置档树形层级**：配置档目录行加「查看」按钮（与"选择=切换当前 profile"区分，当前 profile 默认为查看对象）；查看中 profile 的子资源（检查点/快照清单/插件/启动恢复状态）以缩进+左边线的 `.profile-children` 容器呈现，标注「属于配置档: X」；CheckpointsView 新增 profileFilter（按 item.profile 与 summary.profile_name 过滤）；ProfilePlugins 接受 profile 参数（卸载命令作用于被查看的 profile）
2. **数据归属已证实**：SnapshotSummary 含 profile_name+plugin_count——快照/检查点内容=profile 的插件清单+配置文件，dsh_version 仅元数据 → 层级=配置档→(插件/检查点/快照清单) 全部从属配置档（用户提出的"若基于 harness 则插件在前"不适用）
3. **冷切换状态归位**：UpdatesView 重排=「运行时设置」独立面板（折叠来源/模式+pin 输入+保存）；「上游标签与冷切换」合并面板（tag 拉取/选择/切换 + 分隔线 + 冷切换进度）；确认计划面板与版本槽位保持
4. **Agent 操作收敛**：概览页 Start/Stop/Restart 三键 → 仅「强制重启 Agent」一键（restart 动作）；文案=Agent 随 Launcher 启动退出

## P0 用户反馈修复（2026-09-05，提交 3b3b161）

用户以 GUI 截图反馈 4 项，已全部落地（纯前端，无后端改动）：
1. **来源/安装模式下拉按需展开**：默认收起为「更换运行时来源或安装模式」按钮；UpdatesView 挂载时按需 GET /v1/runtime（不进 8s 轮询），检测到工具缺失自动展开
2. **手动指定运行时路径**：node/pnpm/git 三行从只读改为输入框，保存走既有 POST /v1/config set_runtime 的 pin 字段（ownership=system；留空=清除 pin 交回自动发现，placeholder 显示当前解析路径）
3. **运行时设置+冷切换状态合并**：两 Panel 合一（中间 panel-divider 分隔）
4. **导航层级重构（已对照 desktop 证实层级）**：配置档(原生 pnpm package，dsh.profile.bundles=插件清单)→检查点(每 profile 独立槽位，快照=插件清单+配置文件)→插件列表。删除独立「恢复」导航项；ProfilesView 成为枢纽=配置档目录+嵌入 CheckpointsView(embedded 跳过 PageIntro)+ProfilePlugins(从 RecoveryView 提取)+RecoveryDiagnostics(启动恢复状态/日志尾随保留)。RecoveryView 四标签页组件已删除；p0-runtime-recovery-ui.test.ts 已改写为 profile hub 断言
- 教训：往 App.tsx 加新顶层视图组件必须 export，否则测试 ssrLoadModule 拿到 undefined 报 "Element type is invalid"

## ZCode 独立确认（2026-09-05，P0 合入主线）

- **main 已在 8feeb0b**（codex/nexus-p0/integration 快进合入），P0 全部落地：tag 枚举/槽位容量/一键切换（1e1838b/ec905f3/df3cff0）+ 运行时供给/冷安装/快照恢复/插件卸载（takeover 系列）
- **ZCode 独立复核通过**：在 E:/git/dsh-nexus-phases/p0-integration 实测 cargo test 全 workspace **247 passed / 0 failed**，前端 22 passed / 0 failed，Tauri allowlist 含 /v1/runtime。与 artifacts/takeover/p0-final-acceptance.md 记录一致
- ZCode 曾在 main 上有 P0-4a 检测端点半成品草稿（runtime.rs 探针+路由），确认被 Codex 的实现（observe_* 预算+pin+runtime-supply crate）完全超集覆盖，已丢弃并恢复其 tracked runtime.rs，合并阻塞解除
- 用户已确认浏览器启动 Harness（GUI 实测的一部分）；认证 Web 用系统浏览器回退，iframe 内嵌未主张
- P1/P2 未开始。遗留观察：.tmp-ui-audit/ 未跟踪目录待清理确认；系统级安装分支与 Unix 行为未实测（按红线本就留真机验证）

# dsh-nexus 项目状态与需求基线

## 当前接管阶段：P0 基础集成，完整 P0 仍在执行

- 已独立复核通过：runtime `8261a6e` + `a98beb8`；snapshot engine `0b8fb81` + `9875c84`。运行时 GET/plan 的前置读取、观察和 child cleanup 共享同一 deadline 与全局三许可；snapshot 的必需清单、槽位中断恢复、回滚路径和持久顺序四项问题已修复。
- 隔离基线工作树：`E:/git/dsh-nexus-phases/p0-integration`，分支 `codex/nexus-p0/integration`。原 `main` 及接管前草稿保持不变。来源与组合验证见 `artifacts/takeover/p0-foundation-integration-report.md`。
- 组合验证：Agent 101、Core 30、Launcher Core 11、Protocol 13、Snapshots 17，共 172 项及 doc-tests 通过；CLI offline check 通过。Tauri/前端在后续接线完成后再合批验证。
- `nexus-snapshots` 已接入 Agent checkpoint：新 checkpoint 持有真实内容引用/摘要，支持 manual capture/list/detail/inspect/restore/retry/abort；legacy manifest 明确保持 metadata-only。外层两阶段 journal 绑定实际 DSH_HOME/profile，Prepared 物化失败保持 pending，Retry/Abort 显式决策，启动时 Prepared 回滚、Committed finish。
- 健康 Running/log-session 以持久 once latch 自动捕获一次，默认三健康槽与 manual 保留独立；物化统一消费 runtime pins/env/pnpm args，Windows Job Object 负责 owned descendants 清理。现有界面仍未接恢复四标签页。
- 下一批：运行时供给与确认、实际 cold clone/build/Node 配置；官方插件与原生 Profile 适配；恢复四标签页与实际 Node/GUI 验收。P1/P2 未开始。
- 验收边界：当前是静态/离线/故障注入与真实 Windows junction 验证；未做物理断电、真实安装或真实 Harness/GUI。系统安装产品路径要实现并模拟验证，本机仅允许便携模式实测。
- 手动 capture 中断可能留下无法证明身份的 `.staging-*`，当前 inventory 会拒绝继续并要求明确清理；不得静默选择或删除不明候选。恢复界面需给出此类可操作诊断。

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

## 当前仓库状态（2026-09-05 11:48 更新，隔离集成分支）

- **前序 Codex/ZCode 阶段已结束**：P0-1/P0-2/P0-3 的提交与既有验证记录保留；不再描述旧会话仍在收尾
- **P0-1 已完成（提交 1e1838b feat: enumerate upstream release tags）**：GET /v1/releases/tags（git ls-remote --tags，复用 UpdateSpec.source 校验，timeout 默认 120s）、TagListResponse 协议类型、parse_ls_remote_tags 纯函数+单测（去重/剥 ^{}/倒序）、Tauri 白名单+测试、UpdatesView「上游标签」面板（按钮+下拉+已选显示）、i18n 中英。端到端实测：真实枚举 deepseek-harness 全部 tag（dsh-v0.1.3-alpha.1 最新在前）
- **P0-2 已完成（提交 ec905f3 feat: bound release slots with explicit removal）**：ReleaseStore 加 max_slots（默认3，`with_max_slots`），config.json 新增 `releases.max_slots` section（1-32 校验，agent 启动时读取）；register/register_prepared 满槽返回 ResourceBusy→HTTP 409 `release_slots_full`（消息列出已有槽位）；新 `remove(id)` + `ReleaseAction::Remove`（quiescent 守卫，current/LKG 拒绝→`release_slot_protected` 409）；data_error_response 补 ResourceBusy→CONFLICT 映射；UI 槽位行加释放按钮（current/LKG 隐藏，标记"当前使用/上次可用"）。core 新增 2 个行为测试。端到端实测全过（a:201 b:201 c:409 满，promote 后 rm-current:409 rm-free:200）
- **P0-3 已完成（提交 feat: switch release tag with install and promotion）**：UpdateAction::Switch + UpdateCommand.tag；`UpdateExecutor::switch_tag(tag)`——校验 tag→持 executor gate 持久化 config.ref_name=tag→已装同 version 槽位直 promote（秒切快速路径）→未装则走 install_owned（clone --branch tag→构建→register_prepared）→成功后自动 promote；handler 层 quiescent 守卫（Harness 运行中拒绝 release_change_conflict）；UI 已选 tag 显示"切换到此标签"主按钮。端到端实测：register version=dsh-v0.1.2-rc.1 → switch → current=harness-x，config ref 已更新。nexus-cli UpdateCommand 字段同步
- **隔离集成已完成并等待主控审固定 HEAD**：分支 `codex/nexus-takeover/integration` 从固定基线 `df3cff00cb5e740f223897756f2c7f5bf9c44207` 顺序 cherry-pick 8 个已独立复核提交，全部无冲突；本状态不表示已合并 main、部署或完成 GUI/真实 Harness 验收
- **P0-4a runtime discovery 已集成**：Agent 提供只读 `GET /v1/runtime`；共享 `nexus-launcher-core` 同时允许 `/v1/runtime` 与 `/v1/releases/tags` 的无 body GET，并拒绝 POST 与非空 GET body
- **P0-4 runtime foundation 已在隔离分支实现，尚未合并 main**：`config.json.runtime` 可选保存 node/pnpm/git 的绝对路径与逐 pin `system|nexus` ownership，来源默认 `official`/可选 `npmmirror`，安装模式默认 `portable`/可选 `system`；路径只做结构与 ownership 边界校验，不因文件已被删除而拒绝加载，Settings 后续仍可修复。`NexusPaths.runtimes_dir` 是唯一便携根
- **配置并发已封口**：`ConfigStore::transaction` 用进程级共享粗粒度 Mutex 包住 load→mutate→validate→atomic replace；Agent 的 Set/Clear Harness、Set/Clear Update、Set/Clear Runtime 与 Switch ref 写入均已迁移，不同 `ConfigStore::new` 实例并发写不同字段不会再丢更新；readiness URL 保留语义仍在同一事务内
- **release runtime requirements 与只读计划已实现**：纯解析器只读已注册 release 的 `package.json` 与 `apps/cli/package.json`（单文件 MAX+1 有界读取），支持当前所需 npm `^`、`>=`、`||`、exact 与显式 prerelease 规则，未知语法 fail closed；`POST /v1/runtime/plan` 只接收 `release_id`+source/mode，拒绝未知字段/任意 manifest path，返回 requirements、reuse/missing/incompatible/unverifiable、建议动作和完整 deterministic JSON `plan_id`（明确不是安全 hash，安装前必须重算比对）
- **统一 pins/env/command 下游接口已建立**：配置 pin 优先走现有 6 秒 bounded observation，对真实 canonical path 执行版本探测；失败保留明确 reason，绝不只改 source 假复用。`resolve_runtime_command` 同时覆盖直接 executable 与 pinned Node + pnpm `.js|.cjs|.mjs` entry；`build_runtime_child_env` 只构造 child PATH；`build_pnpm_args` 统一进程局部 minimumReleaseAge=0 与 registry，不改系统/用户环境
- **runtime foundation 独立审查阻断已在 fix1 隔离段修复**：`GET /v1/runtime` 与 `POST /v1/runtime/plan` 都在 handler 最外层创建同一 6 秒 absolute deadline；config、registered release root、两个固定 manifest、PATH/portable 枚举及 shim/cwd 检查全部共享进程级 3 许可 bounded blocking owner，先取许可再 `spawn_blocking`，进入观察和 child/cleanup 时不重置 deadline
- **真实保证边界**：标准 Rust 不能强杀已阻塞的同步文件系统线程；超时任务会继续占用许可直到返回，但永久占位最多 3 个，所有后续请求仍按 deadline 返回。Windows UNC 和映射网络盘候选预拒绝；child 只运行到 `round_deadline - cleanup`，kill/wait 使用同一轮次剩余预算，取消调用方不会取消 detached child owner
- **P0-4a 安全边界已补齐**：PATH/PATHEXT 受限且候选有上限；Corepack canonical/shebang/script 检测 fail-closed，Corepack 不执行并关闭 network/download prompt/default latest/auto pin/project spec；探测 cwd 拒绝祖先 manifest；cmd/bat 采用绝对系统 command processor、拒绝 shell 元字符并隐藏控制台；输出/轮次/子进程/kill+wait 清理有界；数据根与 runtimes 根 reparse point 拒绝，portable canonical 候选必须留在 runtimes 根内
- **P0-1 共享接线缺口已修复**：`nexus-launcher-core` Agent allowlist 补 `/v1/runtime` 与既有 `/v1/releases/tags`，两者只允许无 body GET；POST 和非空 GET body 拒绝；Tauri runtime 移除重复 validator，复用共享 gate
- **手动 runtime 状态面板已集成**：Settings 只在显式 Check/Refresh 时请求 `GET /v1/runtime`；严格解析固定 `git`/`node`/`pnpm` 集合、顺序、可用元数据和绝对路径，失败清除旧成功结果；`system`/`nexus` 来源支持中英文显示
- **Switch 生命周期所有权修复已集成**：显式 Switch 以 supervisor lifecycle → updater gate 固定顺序取得双锁并转交 detached owner；取消请求不会提前释放。冷 Switch 仅在 promotion 后发布 `Succeeded`，命令、promotion、catalog load 或 Agent current-release 最终同步失败均发布 `Failed`；promotion 后的同步失败不回滚 release pointer
- **runtime foundation 组合验证已完成**：原 foundation 的 `cargo test --offline -p nexus-core -p nexus-agent -p nexus-protocol -p nexus-launcher-core` 通过（core 30、agent 97、protocol 13、launcher-core 11，另 doc-tests 全过）且 `cargo check --offline -p nexus-cli` 通过；fix1 新增前置慢 I/O、3 许可饱和后续请求、child/cleanup 不重置 deadline 回归后，`cargo test --offline -p nexus-agent` 101/101 通过。Tauri 未重复生成 resource binary 或做 GUI 总验收
- **仍未验**：未启动真实 Harness，未做真实 cold-install build-to-Node-launch、下载/安装、系统 runtime、GUI 截图/交互、Unix 分支、Windows junction/真实映射盘或部署；未改系统 PATH 或用户配置
- **P0-4 后续仍待做**：当前段没有下载、安装、执行 Corepack、启动 Harness、系统级变更或 GUI 总验收；下游需实现用户确认后的 portable/system 供给与冷安装，并让 install/build/start/terminal/plugin/profile materialization 全部消费上述同一 pins/env/command 接口。规划器暂不扫描未配置的 Corepack cache；本机已知 11.7.0 `bin/pnpm.mjs` 可由下游显式解析后 pin 并安全复用
- **Corepack 官方事实**：官方 `https://github.com/nodejs/corepack` 说明仅 Node `>=14.19` 且 `<25` 随 Node 附带 Corepack；不能假设所有 Node 版本自带 Corepack 或 pnpm 调用不会下载
- 坑：i18n.ts 是 en+zh 两个同 key 对象，勿用全文件 key 去重；本机有钩子会把 	 转义还原成真实 TAB，Rust 里避免 char 转义字面量，用 split_whitespace 类方案
- GUI 截图验收按协议合批：批1（P0-1/2/3/4/8）完成后统一实测
- 历史状态：最新提交原为 683fe8f（34 提交）；nexus-agent doc-test 曾因 target 缓存旧 rlib 报 E0463，重跑即好

- 架构基线：`docs/architecture-baseline.md`（Phase 14；注意其中"默认 127.0.0.1:3090"的表述将随待办15改为 OS 分配，开工时同步修订文档）；阶段报告在 `artifacts/`

## 已知关键实现坐标

- `UpdateSpec.ref_name` 单一字段（默认 "main"）：`crates/nexus-core/src/lib.rs:3863` → tag 枚举要替换的正是这里的产品形态
- release 槽位：`releases/<id>/manifest.json` + `release-pointers.json`（current/last-known-good 原子指针）
- checkpoint：`checkpoints/` manifest + `snapshots/` 七文件内容 + 两阶段 intent journal（`run/`）；legacy manifest 仍仅恢复元数据
- 更新执行器：clone→可选 build/verify→原子发布槽位，任务持久化于 `update-state.json`
- 就绪探针：loopback 纯 HTTP(2xx) 或 `tcp://`（防 SSRF；官方 DSH 根页无 token 返回 401，故 tcp 探针）
- Profile 渲染：`HarnessLaunchSpec.args` 中 `{profile}` / `{release}` / `{release_root}` 占位符
- **共享 runtime 命令基元（快照物化已接线，其余 consumer 待后续）**：`RuntimeConfig` pins + `resolve_runtime_command` + `build_runtime_child_env` + `build_pnpm_args` 是 install/build/start/终端/插件/快照物化的唯一入口；不得在 consumer 复制 PATH、pnpm script 或 registry 参数构造

## P0 runtime supply isolated segment (2026-09-05)

- `nexus-runtime-supply` now provides exact, confirmed supply planning/execution for Windows portable and typed system paths. Plans bind the fresh foundation plan, policy revision, source/mode, host, absolute destination, exact versions, artifact digests/signing identities, cache identity, and ownership. Execution re-derives policy-owned requests and observations; caller URLs/argv are not accepted.
- Reuse order is verified existing pins -> exact read-only Corepack pnpm cache -> complete Nexus-owned cache -> confirmed acquisition. Portable Node ZIP and exact pnpm npm tarball use publisher signature/checksum or npm signature/SRI verification, bounded safe extraction, version probes, same-volume staged atomic publication, directory sync, and concurrent reuse. Child processes use the existing shared runtime command/env primitives.
- System Node MSI and official pnpm user script are represented by fixed internal specs and postflight absolute-path/version probes. Unknown timeout/cancellation returns `NeedsVerification` and no pin. Acceptance used injected runners only; no installer, Corepack shim, PATH/global config change, real DSH home, or Harness was run.
- Offline crate tests pass 15/15 and crate-only strict Clippy passes. Full dependency Clippy remains blocked by seven pre-existing `nexus-protocol` style lints outside this segment. Report: `artifacts/takeover/p0-runtime-supply-report.md`.
- This segment is library-only. Agent persisted orchestration/API/UI confirmation, cold clone/install/build/promotion, remaining consumer wiring, and real portable build-to-launch acceptance remain P0 work.

## P0 cold-install orchestration (2026-09-05)

- Agent now persists one asynchronous cold operation in `cold-operation.json`; `switch` returns 202, `GET /v1/updates` exposes progress/plan/terminal errors, and `confirm`/`cancel` use operation-bound tokens. Awaiting confirmation releases lifecycle/updater ownership and startup preserves only a revalidatable confirmation wait.
- First-run tag GET uses approved `https://github.com/deepseek-ai/deepseek-harness` without creating config. Missing Git fails with manual guidance. Cold clone is a unique Nexus-owned candidate, capacity is rejected before clone, manifests use the bounded runtime planner, and runtime acquisition delegates to the reviewed `nexus-runtime-supply` API.
- Install/build uses the shared pinned command, child env, registry and `minimumReleaseAge=0`, with `--frozen-lockfile`; publication verifies the upstream CLI `bin` contract and `apps/cli/lib/bin.js`, then acquires lifecycle -> updater, rechecks state, registers, writes absolute runtime/Harness config, promotes, and does not auto-start Harness. Promotion synchronization failure restores current/LKG.
- Focused affected checks currently pass: Agent 114, Core 30, Protocol 13, runtime-supply 17, plus CLI build and doc tests. Real isolated upstream build/Node Harness acceptance remains for the root acceptance phase; no real Harness, system installer, global PATH, original DSH home, or GUI was touched here.
