# Project localshelld

## 功能简介

将本机的 “以管理员权限 / root 权限运行任意 powershell 会话” 的能力，同时通过 HTTP API 和 MCP 协议服务出去，会话的开启通过短随机令牌鉴权，用户本人可通过 Master Password 来签发令牌，令牌能且只能由用户亲自签发。令牌及其有效期、撤销状态等授权元数据只在 localshelld 进程内存中维护。需要额外支持用户通过 Authenticator App 来使用动态 Master Password，至少需要兼容 Proton Authenticator。

## Why localshelld

- 用户自然拥有对自己合法所有设备的终极掌控权。localshelld 帮助用户将执行权限临时授权给 AI Agent，授权范围、时长和撤销由用户决定。
- 项目开源，供有经验的设备所有者和维护人员个人自用、自行部署和维护，不面向普通终端用户分发或作为公用服务器服务。
- 唯一的行为约定：尊重用户的个人隐私。

## 技术约束

- 需兼容 Windows 和 Linux
- 需生成 GitHub Actions 用于自动化构建两端的二进制
- 需生成配套 Agent Skill，其中至少要求：
  - 让 AI 在使用 localshelld 时，优先通过大概率集成于 Agent 的 "Ask" tool 来向用户索要 localshelld token。
  - 用户在本地页面使用 Master Password 或动态码签发令牌，Agent 通过令牌接收用户委托。
  - 工具说明仅保留“尊重用户的个人隐私”这一行为提醒。
- 建议分为 2 个模块：
  - server 部分，注册为高权限系统服务，随系统启动而自动启动
  - ui 部分，给用户签发和管理已签发的令牌用
- 当然，能够证明用户身份的关键凭据绝对不要明文存储
- localshelld 提供的服务（不论是 HTTP API 还是 MCP）至少包含以下接口：
  - 进入特权会话（参数：用户给的令牌，返回：会话handle）
    - 特性（写在MCP描述里）：当对应令牌存在未销毁的会话时，不创建新的 而是返回该现有会话的 handle，并在应答中告知复用的事；否则，创建一个新的会话并返回 handle。会话handle同时应当承担密码的职责：handle字符串本身必须是一个合格的强密码。
  - 运行特权 powershell 命令（参数：会话handle、命令字符串）
    - **重要**：这个接口需要着重大量测试，必须做到足够好用、足够全面，例如不能落下对管道、通配符、多行脚本、环境变量等的支持。尽量不要自己实现这些，最理想的形态是我们完全不需要自己处理外部输入，完全交给某个可靠的成熟模块来做执行。
    - 这部分尽量参考成熟实现，例如 OpenCode 的源码（https://github.com/anomalyco/opencode.git）就是一个极佳的范例。
    - 需额外考虑命令的执行身份、环境变量如何继承的问题。
  - 销毁特权会话（参数：会话handle）
  - 销毁令牌（参数：令牌）
  - 用户签发的令牌默认有效期为 24 小时，用户可修改有效期，最长可选永久。不过需要提醒用户：由于令牌记录只存在于进程内存，通常当用户重启系统后，此前签发的任何令牌，不论有效期，都会自然失效。
  - Windows 下区分当前用户、同用户管理员与 LocalSystem；Linux/macOS 高权限身份为 root。

## 用户态 daemon 与权限选择（补充需求）

- 保留高权限系统 daemon，并新增以当前非高权限用户身份运行的 daemon；普通命令由用户 daemon 执行。
- MCP 工具使用 `enter_interactive`、`enter_preauthorized`、`run`、`destroy`、`revoke`；工具名保留 interactive 与 pre-authorized 两类授权的语义。
- Windows 的 `run` 必须显式传入 enum `privilege_level`：`nt-authority/system`、`as-administrator` 或 `current-user`。`as-administrator` 使用同一当前用户的管理员令牌，保留 OS 用户 profile、HKCU、用户级 PATH；不以 SYSTEM 冒充。Linux/macOS 维持 boolean `sudo`。各身份 shell 状态隔离，禁止失败后换身份执行。
- 用户态独立令牌仅允许普通执行；高权限执行仍需用户亲自签发的系统令牌。
- 支持单次字符串 stdin + EOF、分别返回 stdout/stderr。取消或显式超时销毁整个 handle 及关联进程树，返回部分输出和结束原因；断连不自动取消。普通命令非零退出码不销毁会话。
- 结果提供实际身份、毫秒耗时、cwd、截断和会话存活状态；到期时间明确 Unix 秒与 UTC ISO 8601。
- 不提供长任务、后台任务、PTY、后续 write_stdin 或任务轮询系统；任务调度属于 harness。
- 授权防刷需另行讨论策略，本次不调整记忆选择或弹窗策略。

## 环境变量凭据授权（补充需求）

- 后台注册多个 key/value，Key 为大写环境变量名，值加密保存；前后端都验证名称。
- secret 系列公开工具只提供 `secrets_list`，匿名列出 key，不提供获取 value 的接口。
- 两个 enter 均支持可选 `secret_keys`，复用现有执行令牌与交互授权；弹窗额外显示本次 key 列表。
- 授权后按会话保存环境快照，自动注入各身份 shell 及其后代进程，不修改 daemon 的全局环境，不把 value 拼入命令或工具结果。
- 已记忆允许不会无提示扩大到新的 key。预授权执行 token 可选择全部已注册凭据，不新增另一套 secret token。
- 后台轮换或删除不改变已有会话；修改 key 选择须销毁后重新 enter。环境注入减少正常流程的明文传递，但不隔离能主动读取环境的 shell。

## 用户身份与授权

- 将任何能正确提供 Master Password 或动态 Master Password 的个体视为用户本人。

## 技术栈

- 核心层需基于 Rust
- 若需要用 JS 则优先考虑 Bun
- 其它无限制
