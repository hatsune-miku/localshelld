# HTTP 与 MCP API

推荐 base URL：用户 daemon 的 `http://127.0.0.1:32120`。系统 daemon 默认在 `http://127.0.0.1:32119`。授权值通过 JSON body 传递，以下示例使用占位符。

## 普通授权接口

`GET /health` 无需凭据，返回 `status`、`service`、`daemon`（`user` 或 `privileged`）、`version` 和 `commit`。版本为构建时嵌入的 Release 版本（不带 `v` 前缀），本地构建为 `<Cargo版本>-dev`。命令行更新器使用此接口确认服务运行的是预期版本。HTTP/MCP 不提供更新或下载执行接口。

进入会话：

```http
POST /v1/sessions/enter
Content-Type: application/json

{"token":"<LOCALSHELLD_TOKEN>"}
```

返回 `{ "handle": "...", "reused": false, "shell": "powershell", "platform": "windows", "daemon": "user", "sudo_available": true, "expires_at": 1800000000, "message": "..." }`。Linux/macOS 返回 `shell: "bash"`，`platform` 分别为 `"linux"` / `"macos"`。同一 token 存在会话时，`reused` 为 `true` 且 handle 不变。永久令牌的 `expires_at` 为 `null`。

用户 daemon 接受其自身签发的令牌（`sudo_available=false`），也接受系统 daemon 签发的令牌（验证后 `sudo_available=true`）。后一种 handle 关联用户 shell 和高权限 shell。两端的变量、目录、函数和环境完全独立；单一 shell 内串行执行，不同身份的命令可以并行。令牌、Master Password 和 TOTP 不在不同 daemon 的配置文件间共享。

执行命令：

```http
POST /v1/commands/run
Content-Type: application/json

{"handle":"<HANDLE>","command":"Get-Process | Sort-Object CPU -Descending | Select-Object -First 5","sudo":false,"timeout_seconds":30}
```

返回：

```json
{"output":"...","exit_code":0,"success":true,"truncated":false,"session_ended":false}
```

示例命令使用 Windows PowerShell 语法。Linux/macOS 应使用 Bash，例如 `ps -eo pid,comm | head -n 6`。命令中不得包含 NUL 字节，否则返回 400 且会话保持可用。

`sudo` 是必填 boolean，不接受省略、null、字符串或数字。`false` 只在用户 daemon 的账户下运行，`true` 在高权限 daemon 的账户下运行。用户态令牌请求 `sudo=true` 返回 403。直接向系统 daemon 请求 `sudo=false` 时，经显式配对的加密通道转给用户 daemon；未配对返回 503 并提示配对或改用 32120。不会回退到其他身份，也不会重试可能已经执行的命令。HTTP 类型错误返回 422；MCP 参数错误返回 JSON-RPC error。

`timeout_seconds` 省略或为 `null` 时，命令没有执行时间上限；显式传入时必须是正整数，不受原先 300 秒的限制。超时会销毁整个 handle。单 daemon 会话的令牌过期禁止后续调用；跨 daemon 会话还会监听高权限授权到期并终止两端执行。

PowerShell 的 success/error/warning/verbose/debug/information 流通过 `*>&1` 合并为文本。`exit_code` 优先采用原生进程的 `$LASTEXITCODE`；非终止 PowerShell 错误返回 1。Bash 合并 stdout/stderr，返回最后一条命令的退出码（管道遵从当前 `pipefail` 设置）。返回的文本最多保留 `max_output_bytes` 字节，非 UTF-8 字节以替换字符解码。

同一会话的命令串行执行，变量、函数、环境和目录持续保留；调用方断开连接不会遗弃未读响应或自动取消执行。销毁会话、撤销令牌及服务关闭均可取消正在执行及排队中的命令，并向 shell 所在进程组发送信号，一并终止命令启动的子进程和后台任务（主动 `setsid`/新建进程组脱离者除外）。命令不支持交互 stdin；Bash stdin 为 `/dev/null`。

`exit`、`exec` 或 `set -e` 下的失败等使 shell 退出的操作会结束会话。此时本次调用仍返回已产生的输出和 shell 的实际退出码，并将 `session_ended` 置为 `true`；该 handle 之后的调用将失败。正常命令的 `session_ended` 为 `false`。

销毁会话：`POST /v1/sessions/destroy`，body 为 `{ "handle": "<HANDLE>" }`。

撤销令牌：`POST /v1/tokens/revoke`，body 为 `{ "token": "<LOCALSHELLD_TOKEN>" }`。服务仍持有记录的过期 token 可以用于撤销自身。

系统 daemon 还提供两个供用户 daemon 使用的带 handle 认证接口：`POST /v1/sessions/validate` 立即验证；`POST /v1/sessions/watch` 最多等待 15 秒或提前响应会话结束，两者 body 均为 `{"handle":"..."}`。用户 daemon 在普通命令前验证上游授权，并持续长轮询；撤销、到期、上游关闭或通信失败都会取消关联用户 shell。客户端不需要直接调用这些接口。控制请求限时，命令请求沿用显式的 `timeout_seconds`；丢失响应后不会自动重试。

## 管理接口

credential 格式为：

```json
{"type":"password","value":"..."}
```

或：

```json
{"type":"totp","value":"123456"}
```

- `POST /v1/admin/tokens/issue`：`{"credential":...,"ttl_seconds":86400}`。省略有效期默认为 24 小时。永久令牌使用 `{"credential":...,"permanent":true}`。
- `POST /v1/admin/tokens/list`：`{"credential":...}`。只返回 id、签发/过期时间和撤销状态，不返回令牌。
- `POST /v1/admin/tokens/revoke`：`{"credential":...,"id":"..."}`。

Master/TOTP 失败在每个进程实例内限制为 5 次/分钟；成功验证会清除失败计数。

管理接口只管理当前 daemon 的令牌，不转发 Master/TOTP。用户态 UI 签发的令牌永远不允许 `sudo=true`。跨 daemon 令牌可经用户 daemon 的 `/v1/tokens/revoke` 转交签发端撤销。

### 管理登录与凭据设置

`POST /v1/admin/login` 接收 `{"credential":...}`，返回 `{ "session":"...", "expires_at":... }`。管理 session 有效 10 分钟，仅供用户后台使用，不等同于执行 token/handle。下列已登录接口使用 `Authorization: Bearer <session>`。旧签发/列表/撤销接口仍接受 body 中的 credential，也可省略 credential 改用管理 session。

| 接口 | Body / 说明 |
|---|---|
| `POST /v1/admin/logout` | 清除本次管理 session |
| `GET /v1/admin/settings` | TOTP 状态、通行密钥元数据、配对地址、记忆、桌面可用性 |
| `POST /v1/admin/password` | `{"password":"..."}`，允许空字符串；成功后所有管理 session/challenge 失效 |
| `POST /v1/admin/totp/start` | 返回 `enrollment_id`、`secret`、`uri`，5 分钟内确认 |
| `POST /v1/admin/totp/finish` | `{"enrollment_id":"...","code":"123456"}` |
| `POST /v1/admin/totp/remove` | 移除 TOTP，不撤销执行令牌 |
| `POST /v1/admin/passkeys/register/start` | `{"name":"..."}`，返回 challenge ID 和 WebAuthn options |
| `POST /v1/admin/passkeys/register/finish` | `{"challenge_id":"...","credential":<注册结果>}` |
| `POST /v1/admin/passkeys/remove` | `{"id":"..."}` |
| `POST /v1/admin/remembered/reset` | 清空当前授权端保存的所有 agent 选择，返回清除数量 |

通行密钥登录使用 `/v1/admin/passkeys/authenticate/start` 和 `/finish`，无需已有 session；finish 接收 challenge ID 和 WebAuthn credential，验证成功后返回相同管理 session，再调用普通令牌签发接口。challenge 在服务端保留 5 分钟且只能消费一次；注册 challenge 绑定发起它的管理 session。RP ID 固定为 `localhost`，origin 严格绑定当前 daemon 的 `http://localhost:<port>`，要求认证器 user verification。最多 32 个通行密钥，私钥始终留在认证器中。

### 用户进程配对

在两端分别登录后，用户端 `POST /v1/admin/pairing/new` 返回 `code` 和 `user_daemon`。系统端 `POST /v1/admin/pairing/connect` 接收 `{"code":"...","user_daemon":"127.0.0.1:32120"}`，验证对端后保存。`POST /v1/admin/pairing/remove` 仅清除调用端的配对。用户配置的 `privileged_daemon` 必须指向对应系统端。

`POST /v1/bridge` 是配对 daemon 的内部协议，不是公开审批接口。请求和响应使用 AES-256-GCM 认证加密，绑定方向、角色和请求 ID，并检查请求时间与重放；不通过普通 HTTP 接口接收允许/拒绝决定。配对不依赖仅凭端口号认定进程身份。

浏览器请求同时校验 loopback Host 和 Origin 的端口，不开放跨域 CORS；响应禁用缓存。管理 session、配对码和执行凭据不要放入 URL。

## MCP

`POST /mcp` 实现 JSON-RPC 2.0 和 MCP Streamable HTTP 的 JSON 响应，协议版本 `2025-06-18`。支持：

- `initialize`
- `notifications/initialized`（notification 返回 204）
- `ping`
- `tools/list`
- `tools/call`

工具为 `localshelld_enter_interactive`、`localshelld_enter_with_preauthorized_credential`、`localshelld_run`、`localshelld_destroy_session`、`localshelld_revoke_token`。工具说明和 `initialize.instructions` 根据当前平台展示 PowerShell 或 Bash。两个 enter 工具均要求 `confirm_text: "OK"`，仅确认“尊重用户的个人隐私”。旧工具名 `localshelld_enter` 不再接受。服务无 MCP transport session 状态；权限会话由强随机 handle 标识。

长名称为 47 个字符，满足 [MCP 2025-11-25 的工具名建议](https://modelcontextprotocol.io/specification/2025-11-25/server/tools#tool-names)。当前服务仍使用上述 `2025-06-18` 协议版本，不因改名宣称支持新版协议所有特性。

### 交互式进入

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"localshelld_enter_interactive","arguments":{"agent_name":"Example Agent","request_reason":"检查本机开发环境并执行用户要求的构建。","confirm_text":"OK"}}}
```

参数不含 token。`agent_name` 是 1–128 字符、无控制字符的自报名；`request_reason` 非空、最多 16,384 UTF-8 字节、不得含 NUL。缘由作为纯文本显示。结果在 MCP `content` 文本中以 JSON 返回：

- 接受：普通 enter 的会话字段，加 `status: "authorized"`、`btw`；不返回原始 token，也不返回 `automatic` 字段。记忆接受与本次点击接受使用相同中性文案。
- 拒绝：`{"status":"denied","automatic":false,"btw":"..."}`；记忆拒绝为 `automatic:true`。
- 超时：`{"status":"timed_out","timeout_ms":60000,"btw":"..."}`。忽略和关闭弹窗仍等到原始 60 秒截止时间。

请求本身只有授权等待限时，已批准后创建 shell 等处理仍有开销。客户端应给此调用留出超过 60 秒的超时时间。无桌面/配对或通信故障返回明确错误，不产生授权；最多同时等待 8 个请求。窗口展示、HTTP、磁盘 IO 分离，不占用 GUI 消息循环等待网络。

系统入口的交互授权使用已配对用户 daemon 的弹窗；已配对用户入口则转给系统端统一决策，返回双权限会话。未配对用户入口只能批准当前用户权限。内部授权默认 24 小时，记录可在签发端管理页撤销；清除记忆不撤销已有授权。Windows 仅用 Win32 控件，Linux/macOS 的 WebView 仅通过进程内一次性回调提交决定。
