# Windows 自用驱动签名调研

核对日期：2026-10-15。这里只做可行性调研，未购买证书、申请账号、安装驱动或修改 Secure Boot / Test Signing。

## 结论

对 Windows 10/11 桌面版，最接近“仅自用、保留 Secure Boot、不启用 Test Signing、反复更新成本可控”的官方路径，是 **Hardware Developer Program + 有效 EV 代码签名证书 + Microsoft Attestation Signing**。它省去 HLK 测试，但微软当前明确把 attestation 定位为测试用途；不能把它当作没有使用范围限制的生产 WHQL 替代品。[微软签名选项](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings)

如果“自用”是长期生产使用而不是开发测试，需先向微软确认适用条件，或走 HLK/WHCP。没有查到能保证所有条件、无需资格审核且免费的通用个人方案。这里的成本与资格判断是基于公开文档的评估，并非账号准入或驱动获签保证。

## 各路径

| 路径 | 对本项目的判断 |
|---|---|
| Attestation | 支持 Windows 10 Desktop 及以后版本的内核驱动，不要求 HLK；需要硬件开发者账号和关联的有效 EV 证书；仅测试用途，不面向零售 Windows Update 发布 |
| HLK / WHCP | 正式生产兼容认证路线；增加测试、环境与维护成本，适合以后确有生产需求时评估 |
| Preproduction signing | 虽可在配置完成后保持 Secure Boot 开启，但零售系统默认不信任；官方配置流程要求先关闭 Secure Boot、清除密钥再部署测试信任，违反“全程不关闭”的严格条件 |
| 自签名 / Test Signing | 不符合“不启用 Test Signing”的条件 |
| Azure Artifact Signing | 面向用户态签名，不替代 Partner Center 内核签名；不能仅购买低价用户态签名服务解决本问题 |

以上分别依据 [签名选项](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/driver-signing-offerings)、[预生产签名配置流程](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/preproduction-driver-signing-and-install) 和 [Artifact Signing FAQ](https://learn.microsoft.com/en-us/azure/artifact-signing/faq)。Windows Server 2016+ 不应按桌面版的 attestation 路线规划。

## 身份与费用门槛

微软当前注册流程要求组织的 Entra ID 全局管理员、法律联系人、公司信息、EV 证书和审核问卷；没有 D-U-N-S 号码可以手工提交公司资料。购买证书并不自动保证账号获批。[Hardware Developer Program 注册](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/hardware-program-register)

作为价格量级参考，而非最低价推荐：SSL.com 的组织 EV 页面当前标价为一年 **349 美元**，三年方案折合 **249 美元/年**，五年方案折合 **149 美元/年**；多年份是购买期限，不是单张证书有效期。硬件/HSM、云签名订阅、运输税费等另计，具体总价需报价确认。页面特别提示部分 YubiKey 方案可能不满足 EV 内核用途，应在购买前确认兼容性。[CA 官方报价](https://www.ssl.com/products/software-integrity/code-signing/ev/)

该 CA 也提供 Sole Proprietor EV，宣称面向个人/独资开发者并支持内核驱动用途。因此不能简单断言“个人绝对办不了 EV”；但 CA 能签发证书与微软能批准硬件账号是两回事，应先分别确认用户所在地区、主体资料和微软准入条件，再付款。[个人/独资 EV](https://www.ssl.com/products/software-integrity/code-signing/ev-sole-proprietor/)

## 更新与代码约束

驱动每次修改后，需要重新打包 CAB 并提交签名，不能复用旧二进制的签名。注册的 EV 证书必须在提交时有效；可持续复用有效证书对应的提交身份，不需要每次更新重新购买一张证书。实际重复操作可以脚本化，但云签名的次数/订阅费用与证书续费仍需计入。本次未核实到足以承诺“微软每次提交永远免费”的最新收费依据。[证书要求](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-reqs)、[Attestation 提交流程](https://learn.microsoft.com/en-us/windows-hardware/drivers/dashboard/code-signing-attestation)

签名只解决系统信任入口，不保证驱动兼容或稳定。后续实现应仅采用公开 WDK API/DDI 和结构、明确支持的 Windows 版本，并验证 HVCI/内存完整性、安装卸载和失败恢复。不要使用未公开结构、绕过代码完整性或借用有漏洞驱动来达成权限能力。

建议先确认“自用”的实际使用场景及账号资格，再拿最小公开 WDK 示例验证获签和加载全过程；通过后才投入 localshelld 内核后端。当前授权、显示、IPC 与执行层已分离，可复用现有授权模型，不需要现在引入驱动。
