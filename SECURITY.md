# 安全问题报告

[English](SECURITY.en.md)


如果问题涉及越权控制、用户数据删除、密钥泄漏或不受信任输入执行，请不要在公开 Issue 附利用细节、真实凭据或完整诊断包。

仓库启用 GitHub 私密漏洞报告后，请使用 **Security → Report a vulnerability**。若没有该入口，请先创建不含敏感信息的 Issue，请维护者提供私密联系方式；当前文档不代表私密报告入口已启用，也不承诺固定响应时限。

报告可包含 Nexus 版本及构建编号、操作系统版本与 CPU 架构、最小复现步骤、影响范围和脱敏证据。仅测试自己有权操作的本地环境，不扫描他人的 Agent 或修改他人数据。

本地回环地址不等于授权。Nexus Agent 的身份和访问验证属于产品边界；运行的 Harness 和插件不是被 Nexus 隔离的安全沙箱。用户应仅运行自己信任的程序和补丁。

项目目前仍处于早期迭代阶段，尚无长期支持版本承诺。代码签名延期不意味着免除发布来源、产物完整性或第三方许可检查。

## 验证发布下载

发布附件包含聚合校验清单 `SHA256SUMS.txt`，以及它的 Sigstore keyless 签名 `SHA256SUMS.txt.sig` 与证书 `SHA256SUMS.txt.crt`。签名由本仓库的 `release.yml` 工作流在发布时生成，绑定该工作流在对应 tag 上的运行身份。这与操作系统代码签名和 Apple 公证不同：macOS ad-hoc 签名也不等于 Developer ID 或公证。实际签名状态以具体产物记录为准；清单验签证明发布来源，不授予操作系统级信任。

下载全部文件后执行：

```sh
cosign verify-blob SHA256SUMS.txt \
  --signature SHA256SUMS.txt.sig \
  --certificate SHA256SUMS.txt.crt \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity-regexp '^https://github.com/chelal233/dsh-nexus/\.github/workflows/release\.yml@refs/tags/'
sha256sum -c SHA256SUMS.txt
```

需要安装 [cosign](https://github.com/sigstore/cosign)（验证依赖 Rekor 透明日志的联网查询）。逐架构的 `<包名>_SHA256SUMS.txt` 与聚合清单内容一致，可单独核对。
