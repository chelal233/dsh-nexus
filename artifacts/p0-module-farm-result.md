# P0 模块农场修复验证

2026-09-06；基线 433ebfa；任务工作树 E:/git/dsh-nexus-p0-module-farm。

方向可行：启动前维护与当前槽位一致的模块链接，无需修改上游代码。此前“物化器将 rc.1 改回 alpha.1”的推断未获证实。

确定性缺陷：same_directory 比较目标与自身；当前指针 rc.1 但配置写死 alpha.1，run 46/47 堆栈来自 alpha.1；Restart 漏掉 heal，重复 Start 的 heal 又早于运行检查。

修复：比较真实链接目标，安全替换 junction，拒绝覆盖真实目录或异常父目录；失败阻止启动。已登记槽位入口与 cwd 规范成 release_root 占位符，外部命令及用户其他参数保持不变。Start/Restart 统一在运行检查后准备链接，保存配置共用规范逻辑。

验证通过：

- core 35 项、agent 131 项；真实 Windows junction 定向回归 3 项；cargo check --workspace；git diff --check。
- 上游公开 healProfilesModuleFallback：临时 DSH home，alpha.1→rc.1→alpha.1 目标正确。
- 新 Agent 隔离运行：故意使用 alpha.1 硬编码配置、current=rc.1，正确启动 rc.1；再 alpha.1→rc.1 均运行成功，模块目标正确。
- 实际 Web HTTP 200；重复 Start 返回 409 且不动运行中链接；Restart 正确重指；profile manifest 未改变。
- 故意加入不存在的测试 bundle：失败可见，恢复接口包含原因，profiles 接口仍可用。

测试进程已结束。测试未修改用户 .dsh 配置档、插件或上游代码。原始结果在 .tmp-p0/runtime-result.json、runtime-extra-result.json。测试日志可能含临时 Web 凭据，不应公开。临时 releases junction 清理被工具策略拒绝，链接保留。

限制：rc.1 自身缺少部分现有插件要求的 API，托管不等于保证旧插件跨版本兼容。本次交付为源码修复及验证记录，未替换日用 Agent；用户现有 desktop 配置档未做实际运行验收。
