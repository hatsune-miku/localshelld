# HTTP 与 MCP API

推荐用户 daemon：`http://127.0.0.1:32120`；系统 daemon 默认 `http://127.0.0.1:32119`。执行凭据放在 JSON body 中，不放 URL。两端升级时应使用相同版本。

`GET /llms.txt` 在两个入口均无需鉴权，返回 `text/plain; charset=utf-8` 的 AI 易读接口文档，包含调用流程、平台参数、错误与取消语义，以及从实际接口定义生成的输入／输出 JSON Schema。默认地址为 `http://127.0.0.1:32119/llms.txt` 和 `http://127.0.0.1:32120/llms.txt`；仍保留 loopback Host/Origin 限制，不改变执行和管理接口的鉴权。

## 能力发现与 MCP 对应关系

`GET /v1/capabilities` 无需凭据，返回当前 daemon 的 `protocol`、`version`、`commit`、`platform` 和 `operations`。每项操作包含 `name`、`method`、`path`、`inputSchema`、`outputSchema`；schema 与 MCP `tools/list` 共用同一份定义。返回的 `cancellation` 描述取消方式，`interactive_timeout_ms` 为 60000。

| MCP 工具 | HTTP 接口 |
|---|---|
| `secrets_list` | `GET /v1/secrets/keys` |
| `enter_interactive` | `POST /v1/sessions/enter/interactive` |
| `enter_preauthorized` | `POST /v1/sessions/enter/preauthorized` |
| `run` | `POST /v1/commands/run` |
| `destroy` | `POST /v1/sessions/destroy` |
| `revoke` | `POST /v1/tokens/revoke` |

HTTP 请求直接发送 MCP 工具的 `arguments` 对象，响应直接返回对应的 `structuredContent`，不需要 JSON-RPC 外层。两种入口共用参数校验和业务处理；token、handle、记忆选择和凭据授权可以跨入口使用。未知字段、错误类型和不支持的平台参数会被拒绝，不自动转换。HTTP 参数错误返回 400；JSON 格式、Content-Type 等传输错误由 HTTP 层返回 400/415/422。凭据无效返回 401，执行身份越权返回 403，授权通道不可用返回 503，业务错误响应格式为 `{"error":"..."}`。

## 进入与执行

`GET /health` 无需凭据，返回 `status`、`service`、`daemon`、`version`、`commit`。本地构建版本为 `<Cargo版本>-dev`。HTTP/MCP 不提供更新或下载执行接口。

### 预授权

`POST /v1/sessions/enter/preauthorized`：`{"token":"<LOCALSHELLD_TOKEN>","confirm_text":"OK","secret_keys":["JENKINS_CI_TOKEN"]}`。`secret_keys` 可省略，`confirm_text` 必须为 `OK`，与 MCP 一致。

原有 `POST /v1/sessions/enter` 保留兼容，仍接受 `{"token":"<LOCALSHELLD_TOKEN>","secret_keys":["JENKINS_CI_TOKEN"]}`，不要求也不接受 `confirm_text`。

返回 `handle`、`reused`、`secret_keys`、`shell`、`platform`、`daemon`、`privilege_levels`、`identities`、`expires_at`、`expires_at_iso`、`message`。Linux/macOS 另有 `sudo_available`。同一 token 有存活会话时复用同一 handle，`reused:true` 不表示命中记忆授权。`expires_at` 是 Unix epoch **秒**，`expires_at_iso` 是 UTC ISO 8601；永久令牌两者均为 null。

`identities` 按执行身份名返回实际操作系统账户信息。Windows 包含 user、SID、is_system、elevated、integrity、session_id；Unix 包含 user、uid、elevated。仅报告当前可用且授权允许的身份，不以平台名推断 SYSTEM。不同身份的变量、目录和环境独立持久化，不复制已运行 shell 的状态。

### 交互授权

`POST /v1/sessions/enter/interactive` 不需要 token 或管理登录：

```json
{
  "agent_name": "Example App",
  "request_reason": "执行用户要求的本机检查。",
  "confirm_text": "OK",
  "secret_keys": []
}
```

请求进入同一套桌面／Telegram 授权流程，不存在通过 HTTP 提交接受或拒绝的接口。Telegram 仍要求已连接云服务。名字、原因、凭据选择、记忆和权限范围与 MCP 完全相同；`confirm_text` 只是隐私确认，不代表用户已批准执行。

客户端调用超时应大于 60 秒。接受、拒绝和超时都返回 HTTP 200，必须检查 `status`：

- `authorized`：普通 enter 字段加 `status`、`btw`，包含后续执行需要的 `handle`，不返回原始 token。
- `denied`：`status`、`automatic`、`btw`，不包含 handle。`automatic:true` 表示用户先前记忆的拒绝。
- `timed_out`：`status`、`timeout_ms:60000`、`btw`，不包含 handle。忽略或关闭弹窗仍等待原来的截止时间。

`btw` 是用户附言；`authorized` 不代表本次一定发生了新的点击。交互授权目前没有单独的提前取消接口，与 MCP 一样使用固定期限。不要在客户端超时后自动重发，以免重复请求授权。

### 执行

`POST /v1/commands/run` 的 Windows 请求：

```json
{
  "handle": "<HANDLE>",
  "command": "[Console]::Out.Write([Console]::In.ReadToEnd())",
  "privilege_level": "as-administrator",
  "stdin": "hello\n",
  "timeout_seconds": 30
}
```

Windows `privilege_level` 必填，且仅允许：

| 值 | 执行方式 |
|---|---|
| `current-user` | 用户 daemon 的非提权账户；系统入口经配对加密桥转发 |
| `as-administrator` | 已配对当前用户的 linked elevated token；保留 USERPROFILE、HKCU 和用户 PATH |
| `nt-authority/system` | 必须确实由 LocalSystem 执行，不能用普通管理员账户冒充 |

`as-administrator` 不是修改 SYSTEM 环境变量：校验配对用户进程的创建时间、SID、登录 session 和令牌，再使用同一用户的管理员令牌。无 linked token、未配对或所需 OS 权限不足时失败，不换账户执行。管理员 shell 在 broker 的非交互 session 内运行；不提供交互桌面或映射网络驱动器保证。`-NoProfile` 仍然有效：操作系统用户 profile/HKCU 保留，但不自动运行 PowerShell profile 脚本。

Linux/macOS 保持原有 boolean `sudo`，不接受 `privilege_level`：

```json
{"handle":"<HANDLE>","command":"cat; id -u","sudo":false,"stdin":"hello\n","timeout_seconds":30}
```

`sudo:false` 为用户 daemon 的普通账户，`sudo:true` 为 root。用户端自身签发的令牌在所有平台都只能执行当前用户命令。错误身份不会自动回退；不重试可能已执行的命令。缺失或类型不正确的字段返回 HTTP 400/422；无法取得请求身份或无权限返回明确错误。Windows 的旧 boolean `sudo` 被拒绝，即使同时提供正确的 `privilege_level`。

`stdin` 是可选 UTF-8 字符串，按字节供本次命令读取，写完即 EOF；省略等于空输入。允许 stdin 内含 NUL，但 command 内 NUL 被拒绝。子程序负责按其自身编码约定解码；本服务不重写用户命令。不提供后续 write_stdin、PTY、实时输出、后台任务或任务轮询接口。

正常返回示例（identity 内账户为示意）：

```json
{
  "stdout":"hello\n","stderr":"","output":"hello\n",
  "exit_code":0,"success":true,
  "truncated":false,"stdout_truncated":false,"stderr_truncated":false,
  "duration_ms":120,"timed_out":false,
  "session_alive":true,"session_ended":false,"session_ended_reason":null,
  "cwd":"C:\\Users\\owner",
  "identity":{"user":"PC\\owner","sid":"S-1-5-21-...","is_system":false,"elevated":true,"integrity":"High","session_id":0}
}
```

stdout、stderr 分别捕获，各最多保存 `max_output_bytes` 字节；非 UTF-8 输出用替换字符解码。超过上限继续排空，以维持会话协议。旧 `output` 兼容字段是 stdout 后接 stderr，不表示两者的时间交错顺序。PowerShell error 流进入 stderr，其它逻辑流暂不提供独立字段。原生程序 stderr 有文本不意味着失败；以 exit_code/success 判断。PowerShell 保留原生进程退出码，PowerShell 非终止错误返回 1；Bash 遵循最后命令及当前 pipefail 设置。

`cwd` 是完成时 shell 工作目录，不能在中断后可靠获取时为 null。`identity` 是承载脚本的 shell 的真实 OS 身份，不是脚本自行另起的任意程序身份。`duration_ms` 为该命令开始执行后的耗时，不含串行队列等待时间。

## 超时、断连与取消

- `timeout_seconds` 省略或 null 时无默认执行超时；显式值为正整数，不设旧的 300 秒上限。
- **普通命令非零退出码不销毁会话**。直接执行 `exit`、替换 shell 的 `exec`、或 Bash `set -e` 引起 shell 退出时，会结束整个 handle。
- **执行超时销毁整个 handle**，终止所有关联身份的 shell/进程树。返回已捕获的 stdout/stderr、`timed_out:true`、`session_alive:false`、`session_ended:true`、`session_ended_reason:"timeout"`、`exit_code:null`；不伪造退出码，也不承诺保留被中断的 shell 状态。
- `session_ended_reason` 为 `shell_exited`、`timeout`、`cancelled` 或 null。`cancelled` 包括显式取消、销毁、撤销与关闭导致的中断；队列中尚未开始的命令可能返回会话已结束错误。
- **HTTP 断连或客户端自身超时不自动停止命令**。worker 继续消费输出，避免下次调用串包；客户端不得因响应丢失而盲目重试有副作用的命令。
- `POST /v1/sessions/destroy`，body `{"handle":"<HANDLE>"}`，显式销毁会话。`POST /v1/tokens/revoke`，body `{"token":"<TOKEN>"}`，撤销并清理该令牌所有会话。服务仍持有记录的过期 token 可以撤销自身。
- HTTP 客户端需要取消执行中的命令时，从另一请求调用 `sessions/destroy`。效果与 MCP run 的取消通知一致：整个 handle 及其关联进程树终止，不只是某一条命令。HTTP destroy 返回 `{"destroyed":true}`；仍保持连接的 run 返回已捕获结果和 `session_ended_reason:"cancelled"`，而 MCP 取消通知采用协议规定的 204 响应。
- Windows shell 在恢复执行前就加入禁止 breakaway 的 kill-on-close Job Object，销毁时清理其整棵进程树。Unix 清理可追踪后代和原始进程组；主动 double-fork 后完全脱离、被重新托管的 daemon 不在保证范围内。
- 单 daemon 令牌到期阻止后续调用；跨 daemon 会话的授权到期监视还会取消关联命令。系统端内部 `/v1/sessions/validate` 与 `/v1/sessions/watch` 使用 handle 验证，watch 最多等 15 秒；客户端无需直接使用。

## Windows HTTP 调用示例

以下示例请求交互授权，以当前用户执行 `whoami`，随后销毁会话；不打印 token 或 handle：

```powershell
$apiBase = 'http://127.0.0.1:32120'
$approval = Invoke-RestMethod -Method Post -Uri "$apiBase/v1/sessions/enter/interactive" `
  -ContentType 'application/json' -TimeoutSec 75 -Body (@{
    agent_name = 'Example App'
    request_reason = '检查当前执行账户。'
    confirm_text = 'OK'
  } | ConvertTo-Json)
if ($approval.status -ne 'authorized') {
  throw "Authorization result: $($approval.status)"
}
try {
  $result = Invoke-RestMethod -Method Post -Uri "$apiBase/v1/commands/run" `
    -ContentType 'application/json' -Body (@{
      handle = $approval.handle
      command = 'whoami'
      privilege_level = 'current-user'
      timeout_seconds = 10
    } | ConvertTo-Json)
  $result.stdout
} finally {
  Invoke-RestMethod -Method Post -Uri "$apiBase/v1/sessions/destroy" `
    -ContentType 'application/json' -Body (@{ handle = $approval.handle } | ConvertTo-Json) | Out-Null
}
```

## 管理接口

### 环境变量凭据

`GET /v1/secrets/keys` 无需授权，返回 `{"keys":["JENKINS_CI_TOKEN","OPENAI_API_KEY"]}`，只列名称。没有公开读取 value 的接口。

两个 MCP enter 工具及 HTTP enter 均接收可选 `secret_keys: string[]`。key 同时是环境变量名，符合 `[A-Z_][A-Z0-9_]{0,127}`；后端同样校验大写。最多保存及请求 64 个 key，重复请求名会去重排序。value 为 1–16,384 UTF-8 字节，不能含 NUL。`LOCALSHELLD_*`、`BASH_ENV`、`ENV`、`SHELLOPTS`、`BASHOPTS`、`CDPATH` 为 shell 运行时保留名称。

- 交互弹窗显示本次请求的完整 key 列表，不显示 value。记忆允许仅覆盖已确认的 key；旧的 shell 允许记忆或新增 key 仍需再次确认。记忆拒绝继续拒绝该 agent 的请求。
- 预授权使用原有执行 token，不另签发 secret token。同一有效 token 可以选择任意已注册 key；执行身份权限不变。
- 新会话省略或传空列表时不注入凭据。存活会话重新 enter 且省略或传空列表时保留原选择；显式传不同的非空列表会报错，须先 destroy 再进入。
- 注入发生在子进程创建时，不拼接 shell 赋值语句，不修改 daemon 的全局环境。当前用户、管理员和 SYSTEM/root 的 shell 使用同一份会话快照，子进程正常继承。各身份执行后的环境变更仍彼此独立。
- 后台更换或删除凭据不改变已有快照。销毁后重新 enter 才读取新值，且需再次提供所需 key；未知 key 使新建失败，不创建部分会话。
- 配对后统一使用用户 daemon 的凭据库，两端管理页通过已有加密桥访问同一库。未配对时使用本 daemon 的库；配对不会复制或迁移原独立库。系统端无法连接已配对的凭据库时不会回退到另一份值。

这些值不由列表、enter 或会话元数据回传；管理页的显式文件导出会返回明文值。获得环境变量的 shell 及其子程序仍能主动打印、记录或发送它们，`run` 不提供防泄露隔离或可靠的任意输出脱敏。

下列管理接口要求现有 10 分钟管理 session 的 Bearer header，执行 token 不能代替管理登录：

| 接口 | Body / 返回 |
|---|---|
| `GET /v1/admin/secrets` | `{"keys":["JENKINS_CI_TOKEN"]}` |
| `POST /v1/admin/secrets/put` | `{"key":"JENKINS_CI_TOKEN","value":"..."}`；创建或替换，返回 `{"saved":true}` |
| `POST /v1/admin/secrets/remove` | `{"key":"JENKINS_CI_TOKEN"}`；返回 `{"removed":true}` |
| `POST /v1/admin/secrets/export` | `{}`；返回 `format`、`version`、`credentials`，包含明文值，附件文件名为 `localshelld-credentials.json` |
| `POST /v1/admin/secrets/import` | 完整凭据文件对象；校验全部内容后整库替换，返回 `{"imported":true}` |

文件格式及云端协议见 [SYNC.md](SYNC.md)。管理接口还包括 `POST /v1/admin/sync`：

| `action` | 其他字段及返回 |
|---|---|
| `status` | 返回 `url`、`revision`、`last_sync`、`error`；离线时 url 为 null，不返回 token |
| `start` | 传 `url`、`name`，返回 `verification_uri`、`user_code`、`expires_in`、`interval`；私有 device code 留在 daemon |
| `poll` | 等待授权时返回 `{"pending":true}`；成功拉取并保存云端数据后返回 `{"connected":true}` |
| `refresh` | 立即从云端读取，返回 `revision` |
| `disconnect` | 保留最后确认的本机数据并切换离线，返回 `{"offline":true}`；尝试撤销远端会话 |

这些操作同样要求管理登录，不接受执行 token。已配对时转发至用户 daemon。仍配置云端但无法访问时，凭据列表、读写、导入导出和新会话的凭据解析失败，不使用离线缓存回退；已授权 shell 的环境变量快照保持不变。

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

管理接口只管理当前 daemon 的令牌，不转发 Master/TOTP。用户态 UI 签发的令牌永远只允许当前用户执行。跨 daemon 令牌可经用户 daemon 的 `/v1/tokens/revoke` 转交签发端撤销。

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

`POST /mcp` 使用 JSON-RPC 2.0、MCP Streamable HTTP JSON 响应，协议版本 `2025-06-18`。支持 initialize、ping、tools/list、tools/call，以及 notifications/initialized、run 的 notifications/cancelled。

工具列表只公布六个短名称：

| 工具 | 用途 |
|---|---|
| `secrets_list` | 无需授权列出凭据 key，不返回 value |
| `enter_interactive` | 桌面／Telegram 交互式请求授权，无 token 参数 |
| `enter_preauthorized` | 使用用户事先签发的 token 进入 |
| `run` | 显式选择身份，执行单次命令并传入可选 stdin |
| `destroy` | 销毁会话及所有关联进程树 |
| `revoke` | 撤销 token |

旧 `localshelld_enter_interactive`、`localshelld_enter_with_preauthorized_credential`、`localshelld_run`、`localshelld_destroy_session`、`localshelld_revoke_token` 暂保留调用入口兼容，不重复列入 tools/list。Windows 参数仍必须升级为 privilege_level。更早的 `localshelld_enter` 不恢复。每个工具提供 inputSchema、outputSchema；结果同时有 content 文本和 structuredContent。

### 客户端取消

initialize 响应通过 HTTP `Mcp-Session-Id` 返回强随机传输会话标识。客户端应在后续请求和通知中回传该 header。它只用于绑定请求归属，不替代 token/handle 执行授权。没有 header 的旧客户端仍可调用工具，但不能仅凭裸 requestId 取消命令。

```json
{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":17,"reason":"User cancelled"}}
```

服务器只取消同一传输会话中仍在执行的 run，销毁其整个 handle 并清理进程树；取消通知与被取消的 HTTP 请求返回 204，不再给被取消请求发送工具结果。未知或已完成的 requestId、其他客户端的同号请求、没有归属信息的通知均不影响执行。交互授权仍使用固定 60 秒期限，不由此通知取消。闲置传输会话可回收；传输会话失效不会自动撤销执行 handle。当前无后台任务 API。

### 交互授权与预授权

```json
{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"enter_interactive","arguments":{"agent_name":"Example Agent","request_reason":"执行用户要求的本机检查。","confirm_text":"OK"}}}
```

`enter_preauthorized` 参数为 `{"token":"<TOKEN>","confirm_text":"OK"}`。两个入口的 confirm_text 仅确认尊重用户个人隐私。需要注入凭据时添加 `"secret_keys":["JENKINS_CI_TOKEN"]`，后续 PowerShell 使用 `$env:JENKINS_CI_TOKEN`，Bash 使用 `$JENKINS_CI_TOKEN`，无需获取明文。

agent_name 是 1–128 字符的自报名，不得有控制字符；request_reason 非空、最多 16,384 UTF-8 字节、不得有 NUL，按原文显示。交互结果：

- 接受：普通 enter 字段，加 `status:"authorized"`、`btw`；不返回原始 token，不声称一定发生本次点击。
- 拒绝：`{"status":"denied","automatic":false,"btw":"..."}`；记忆拒绝为 automatic:true。
- 超时：`{"status":"timed_out","timeout_ms":60000,"btw":"..."}`。忽略、关闭弹窗仍等至原 60 秒截止时间。
- btw 是用户在弹窗填写的附言，类型为 string，不是无来源的服务端噪音。

客户端应给交互授权留出超过 60 秒的调用时间。无桌面/配对或通信失败时明确报错。最多同时等待 8 个请求；授权频率冷却与客户端凭据绑定尚未改变。名称只是自报显示信息，同名请求共享记忆，不是身份认证。

配对用户入口转由系统端统一决策；未配对用户入口仅允许当前用户权限。内部交互授权默认 24 小时，可在签发端管理页面撤销。清除记忆不撤销已存在授权。token 和 handle 是 bearer 凭据，可能保存在客户端调用历史中，不应再次回显、写日志或放入 URL。
