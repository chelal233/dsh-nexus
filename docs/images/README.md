# 界面截图

[English](README.en.md) · [返回项目介绍](../../README.zh-CN.md)

## 采集记录

六张截图于 **2026-10-05（UTC）** 从真实 Nexus 窗口采集。使用官方 v1.0.5 Linux x64 DEB 原样解包后的程序，以普通非 root 用户运行；没有安装到系统包管理器，也不是开发预览或重绘界面。

| 项目 | 实际记录 |
| --- | --- |
| 采集日期与时区 | 2026-10-05，UTC |
| Nexus 版本与完整提交 | v1.0.5 / `e72b324934b174f1702b5111af87cb460f661432` |
| 构建标识 | `electron-37218933001-1-x86_64-unknown-linux-gnu` |
| 操作系统与架构 | Debian GNU/Linux 13，x86_64；Intel Xeon Platinum 8573C |
| 程序来源 | 官方 Linux x64 DEB，文件保持原样解包运行；未执行系统安装 |
| 窗口与图片尺寸 | 1180 × 812 像素，无裁剪或缩放 |
| 缩放与主题 | 页面缩放 100%；主题跟随系统，实际显示为浅色 |
| 运行状态 | Nexus Agent 在线；Harness 未安装、未运行；浏览器模式；空的隔离 `web` 配置档 |
| 数据与敏感信息检查 | 隔离的首次使用数据，无凭据或真实用户会话；六张图片已检查 |
| 页面与语言 | 实际点击引导、工作台、设置；通过界面切换英文与简体中文 |
| 图片处理 | 原生窗口捕获的 PNG 转为同尺寸 JPEG（质量 94）；无界面改写、合成或生成内容 |

来源：[v1.0.5 Linux x64 DEB](https://github.com/chelal233/dsh-nexus/releases/download/v1.0.5/dsh-nexus_1.0.5_linux_x64.deb)、[平台构建记录](https://github.com/chelal233/dsh-nexus/releases/download/v1.0.5/dsh-nexus_1.0.5_linux_x64_build.json)、[平台校验清单](https://github.com/chelal233/dsh-nexus/releases/download/v1.0.5/dsh-nexus_1.0.5_linux_x64_SHA256SUMS.txt)。采集使用的 DEB SHA-256 为 `b693d2a14fc141a1ce3e841ee6b67ea4459f036da47c59b4f92fce8fe0842159`，与发行附件记录一致。

采集时自动更新检查遇到云环境的证书信任错误，未绕过该错误，也未下载 Harness。该次采集不作为在线更新下载或安装成功的证据。

## 图片

| 页面 | 中文 | English |
| --- | --- | --- |
| 初始引导 | [guide-zh.jpg](guide-zh.jpg) | [guide-en.jpg](guide-en.jpg) |
| 工作台 | [workbench-zh.jpg](workbench-zh.jpg) | [workbench-en.jpg](workbench-en.jpg) |
| 设置 | [settings-zh.jpg](settings-zh.jpg) | [settings-en.jpg](settings-en.jpg) |

## 如何理解截图

截图展示官方发行程序的首次使用界面与真实状态，不作为 Harness 启动、完整会话、离线迁移、系统安装／升级、通知投递或跨平台验收的证据。

本次截图为 Linux Web 模式：当前内置 Harness 锁没有官方 Linux Desktop 资源，且采集环境尚未安装 Harness。其他平台的 Desktop 入口取决于所选版本与运行时能力，不能仅凭这些截图判断。设置图展示页面顶部，其余选项需要滚动查看。

## 更新要求

- 使用隔离数据，不包含真实会话、凭据、私人路径或未审查的诊断内容。
- 采集真实应用界面；切换语言并等待渲染完成，核对页面标题、导航、当前状态与完整画面。
- 中文和英文覆盖相同页面；记录实际主题、尺寸与缩放，不虚构运行中的 Harness 或成功状态。
- 只进行如实记录的图片格式转换、缩放或裁剪，不使用生成或合成方式添加功能。
- 更新六张图片及中英文说明，补齐版本、提交、来源、运行环境与实际状态，完成核对后再发布。
