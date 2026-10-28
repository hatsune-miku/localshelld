# 凭据同步协议 v1

localshelld 默认使用本机离线凭据库，也可以连接自托管同步服务。此协议同步凭据和 Telegram bot 配置，不同步执行令牌、Master Password、TOTP、桌面授权记忆或本机通行密钥。

参考实现位于 `crates/sync-server`，共享数据类型与校验位于 `crates/sync-protocol`。实现采用单实例、单所有者模型；不同客户端可以按本文独立实现。

本机凭据注入另有保留环境变量限制：PATH、HOME、系统目录、动态加载器、运行时启动钩子及代理等控制变量不可通过凭据创建、导入或注入。旧同步快照可以读取和移除这些键，但执行器仍会拒绝注入；服务端兼容读取不代表客户端允许执行。

## 使用

1. 在本机“凭据”页导出需要保留的数据。
2. 部署后端，打开其 HTTPS 地址。读取服务器数据目录的 `bootstrap-code`，创建第一个通行密钥。
3. 在本机“凭据 → 存储方式”填入后端地址和设备名称，选择“连接服务”。
4. 打开授权链接，使用后端的通行密钥登录，核对本机显示的设备验证码，再授权设备。
5. 客户端下载云端整库并替换本机。新服务的云端库为空；需要迁移原数据时，再导入第 1 步的文件。

通行密钥属于后端的域名，不属于 localhost。另一台设备可以使用已同步的通行密钥、跨设备验证，或由已登录的浏览器添加另一个通行密钥。私钥不上传至服务端。

## 一致性

服务端是唯一的数据权威。每份凭据库有递增的 `revision`，从 0 开始。

- 读：客户端向云端读取完整快照，校验后覆盖本机加密缓存，再返回结果。
- 写：先读取最新快照，在其上应用修改，携带 `expected_revision` 整库提交。服务端原子检查版本、持久化、增加版本，再返回确认快照。客户端保存确认快照，并再次拉取云端。
- 冲突：返回 409，不覆盖另一端的写入。客户端拉取最新快照并要求用户重试，不自动重放旧整库。
- 实时通知：在线客户端长轮询版本变化；提交成功立即唤醒等待者，客户端随后拉取整库。重连和每次心跳也重新读取云端。
- 断网：仍配置云端时，凭据列表、导出、修改及新会话的凭据解析失败，不使用旧缓存授予新凭据，也不积压离线写入。
- 离线模式：用户明确切换为离线存储后，保留最后确认的本机快照，停止同步。

保证的是服务端读写的顺序一致和客户端成功操作以云端为准。网络存在延迟，断线设备的磁盘缓存不可能与云端在同一瞬间变化；它不会被当作最新数据使用。已经授权的 shell 保留创建时的环境变量快照，不会因同步而改写正在运行的程序。

网络错误可能发生在云端提交成功之后。客户端会尝试重新读取，仍无法确认时报告错误；不能据此推断云端没有变化，也不能盲目重试。

## 通用约定

- Base URL 为 HTTPS origin，不含路径、用户名、密码、query 或 fragment。本地开发允许 `http://localhost:<port>`。
- 接口前缀为 `/sync/v1`，请求与响应采用 UTF-8 JSON。凭据及 token 不放入 URL。
- 访问认证使用 `Authorization: Bearer <access_token>`。不使用 cookie，不开放跨域读取。带 `Origin` 的请求必须与配置的 origin 完全匹配。
- 时间为 Unix epoch 秒。客户端应以服务端返回的有效期为准。
- HTTP 400/422 表示请求不合法，401 表示认证无效，403 表示权限不足，409 表示版本冲突，429 表示需要退避。业务错误格式为 `{"error":"说明"}`；框架的 JSON 格式错误可能返回普通文本。
- 请求体上限 8 MiB；客户端也必须限制响应体大小。参考实现不记录请求体或 token，响应设置 `Cache-Control: no-store`。

`GET /health` 返回 `{"service":"localshelld-sync","protocol":1}`。

`GET /sync/v1/info` 无需认证：

```json
{ "protocol": 1, "initialized": false, "origin": "https://sync.example.com" }
```

## 初始化和通行密钥

首次启动生成 256 bit 随机初始化码，保存在私有数据目录的 `bootstrap-code` 文件；状态库保存其哈希。初始化码不通过 HTTP 返回，不写入日志。第一个通行密钥注册成功时，原子消耗初始化码并移除文件；并发初始化只能有一个成功。

### 注册

`POST /passkeys/register/start`：

```json
{ "name": "我的通行密钥", "bootstrap_code": "<INITIALIZATION_CODE>" }
```

初始化后，不再接受初始化码授权。添加通行密钥需要有效的浏览器登录 JWT，`bootstrap_code` 可以省略。设备同步 JWT 不能添加或移除通行密钥。

返回 `{"challenge_id":"...","options":{"publicKey":{...}}}`。按照 WebAuthn JSON 格式把 challenge、user.id、credential id 从 base64url 解码后传给 `navigator.credentials.create()`。

`POST /passkeys/register/finish`：

```json
{
  "challenge_id": "...",
  "credential": {
    "id": "...",
    "rawId": "...",
    "type": "public-key",
    "response": { "attestationObject": "...", "clientDataJSON": "...", "transports": ["internal"] },
    "extensions": {}
  }
}
```

二进制字段使用不带 padding 的 base64url。追加注册的 finish 必须使用 start 所属的同一浏览器会话。挑战仅在服务端保存，5 分钟过期，一次使用，校验 RP ID、origin、challenge 和用户验证。最多 32 个通行密钥。

成功返回浏览器登录 JWT。

### 登录

1. `POST /passkeys/authenticate/start`，body `{}`，取得相同结构的 challenge/options。
2. 使用 `navigator.credentials.get()`。
3. `POST /passkeys/authenticate/finish`：

```json
{
  "challenge_id": "...",
  "credential": {
    "id": "...",
    "rawId": "...",
    "type": "public-key",
    "response": { "authenticatorData": "...", "clientDataJSON": "...", "signature": "...", "userHandle": null },
    "extensions": {}
  }
}
```

成功返回浏览器登录 JWT。认证结果更新已保存的通行密钥状态。

### JWT 和续期

响应格式：

```json
{
  "access_token": "<JWT>",
  "refresh_token": "<OPAQUE_REFRESH_TOKEN>",
  "expires_at": 1900000300,
  "refresh_expires_at": 1902592000
}
```

参考实现使用 HS256，固定验证签名、算法、`iss`、`aud`、`sub`、`exp` 和仍有效的服务端 `sid`。`iss` 为配置的 origin，`aud` 为 `localshelld-sync`。JWT 不包含凭据值。

- 浏览器登录有效期为 5 分钟，`refresh_token` 为空。浏览器仅将 JWT 保存在页面内存；刷新页面或到期后重新验证通行密钥。信任管理操作要求这种近期验证过的浏览器会话。
- 设备访问 JWT 有效期为 5 分钟；refresh token 最多有效 30 天，从设备授权时计，不无限滑动延期。
- `POST /session/refresh`，body `{"refresh_token":"..."}`，无需访问 JWT；返回一对新的 token，旧 refresh token 立即失效。
- 服务端保存 refresh token 哈希和已消费历史。重放已消费 token 会撤销整个设备会话，其访问 JWT 也随即失效。伪造一个不存在的 token 不会撤销已知会话。
- 同一客户端必须串行续期，并立即持久化新 token。续期响应丢失或本地持久化失败时，重新授权设备，不能循环使用旧 refresh token。参考客户端将 token 加密存储。

此实现不是 OAuth/OIDC 服务；设备授权流程借鉴其交互方式，但端点和字段以本文为准。

## 设备授权

1. 客户端 `POST /device/start`，body `{"name":"我的电脑"}`，无需认证。返回：

   ```json
   {
     "device_code": "<PRIVATE_DEVICE_CODE>",
     "user_code": "A1B2C3D4E5F6",
     "verification_uri": "https://sync.example.com/#device=A1B2C3D4E5F6",
     "expires_in": 600,
     "interval": 3
   }
   ```

2. `device_code` 仅保留在客户端进程内，不传给页面或放入 URL。页面只显示 `user_code`，并打开同源的 verification URI。
3. 用户在后端页面登录。浏览器 JWT 调用 `POST /device/inspect`，body `{"user_code":"..."}`，返回 `name`、`user_code`、`approved`。用户核对设备名和本机验证码后，用相同 body 调用 `POST /device/approve`。
4. 客户端每 3 秒 `POST /device/poll`，body `{"device_code":"..."}`。未批准返回 HTTP 202 和 `{"pending":true}`；批准后返回设备 JWT/token，并一次性消费请求。过期、已使用或不存在的 code 返回 400，过快轮询返回 429。
5. 客户端取得 token 后读取 `/vault`，原子保存云端快照和同步配置。此前不覆盖本机库。

任何已授权设备都可读取、替换完整云端凭据库，但不能凭设备 JWT 管理其他设备或通行密钥。

`GET /sessions` 要求浏览器 JWT，返回 `passkeys:[{id,name}]` 和 `sessions:[{id,name,created_at,expires_at}]`；后者只列出仍有效的设备会话。

`POST /session/revoke`，body `{"id":"<SESSION_ID>"}`。浏览器可撤销指定设备；设备可传空 id 撤销自身。撤销即时作用于 JWT 验证，也唤醒通知连接。

`POST /passkeys/remove`，body `{"id":"<PASSKEY_ID>"}`，要求浏览器 JWT，不允许移除最后一个通行密钥。移除通行密钥不等同于撤销已授权设备，应分别操作。

## 凭据整库

`GET /vault` 要求有效浏览器或设备 JWT，返回：

```json
{ "revision": 7, "credentials": { "JENKINS_CI_TOKEN": "...", "OPENAI_API_KEY": "..." } }
```

`PUT /vault`：

```json
{ "expected_revision": 7, "credentials": { "JENKINS_CI_TOKEN": "new value" } }
```

这是整库替换，不是 merge；示例会删除未包含的 `OPENAI_API_KEY`。成功返回新完整快照，例如 `revision:8`。空对象清空库。版本不匹配返回 409，任何写入都不发生。

所有实现必须遵守相同校验：最多 64 个 key；key 符合 `[A-Z_][A-Z0-9_]{0,127}`；value 为 1–16,384 UTF-8 字节，不含 NUL。禁止 `LOCALSHELLD_*`、`BASH_ENV`、`ENV`、`SHELLOPTS`、`BASHOPTS`、`CDPATH`。整份请求先校验，再原子提交，不部分导入。

## 实时通知

`GET /events?after=7` 要求访问 JWT。版本不同立即返回 `{"revision":8}`；相同则等待变化，最多 25 秒后返回当前版本。响应前再次检查会话有效性。通知不包含凭据值。

客户端收到响应后拉取 `/vault` 并再次订阅。连接中断时退避重连；客户端必须保证即使漏掉通知，下一次读取仍从云端取值。v1 使用 HTTP 长轮询，不要求 WebSocket。

## 离线凭据文件

文件是普通 JSON，不加密，不附签名。使用者自行保密。

```json
{
  "format": "localshelld-credentials",
  "version": 1,
  "credentials": { "EXAMPLE_TOKEN": "example value" }
}
```

不包含服务器地址、JWT、执行 token、WebAuthn 私钥或 revision。导入前完整校验格式、版本和所有 key/value；导入会替换全部凭据。连接云端时，导入也通过 CAS 写入云端，不只修改本机缓存。不声称兼容 TOTP authenticator 的导出格式。

本机导入导出只接受管理页面的登录 session，MCP 执行 token 不能调用。`secrets_list` 返回 key 列表；显式 `secrets_get` 及 HTTP `POST /v1/secrets/get` 只返回存活会话已授权 key 的明文快照，不提供整库导出或未授权 key 读取。

## 部署参考实现

Linux 构建需要 Rust、Node.js、pnpm、C 编译器、OpenSSL 开发包和 pkg-config，不需要桌面 GTK/WebKit 依赖：

```bash
pnpm --dir web install --frozen-lockfile
pnpm --dir web build
cargo build --locked --release -p localshelld-sync-server
sudo bash deploy/sync/install.sh "$PWD/target/release/localshelld-sync-server" sync.example.com
```

安装脚本要求已有 Nginx、Certbot、systemd，并且域名已解析至当前服务器、80/443 可达。它只配置独立的 `localshelld-sync` 站点和服务，使用 Certbot webroot 获取证书并登记 Nginx reload hook。应确认系统已有 Certbot 续期 timer，必要时启用对应的发行包或 Snap timer。

服务以独立的 `localshelld-sync` 系统账户运行，只监听 `127.0.0.1:32121`。Nginx 提供 TLS、请求限流和连接数限制。程序与数据分别位于 `/opt/localshelld-sync`、`/var/lib/localshelld-sync`。初始化码：

```bash
sudo cat /var/lib/localshelld-sync/bootstrap-code
```

检查：

```bash
systemctl status localshelld-sync.service
curl --fail https://sync.example.com/health
sudo certbot renew --cert-name sync.example.com --dry-run
```

数据目录权限 0700，密钥与状态文件权限 0600。状态使用 AES-256-GCM 整体加密、原子替换并 fsync；同一目录只能由一个进程持有。`master.key` 缺失、状态损坏或被占用时拒绝启动，不创建空库覆盖。JWT 签名密钥和凭据都包含在加密状态中。Windows 构建用于开发测试；生产部署模板面向 Linux。

这不是端到端加密：后端进程及掌握服务器数据和 master key 的管理员可以解密凭据。备份应包含 `state.aes` 与对应的 `master.key`，并单独做好权限与保密管理。丢失全部通行密钥后，旧初始化码不能重新启用；从可用备份恢复，或使用独立的新数据目录重新初始化，再导入凭据文件。

升级替换二进制并重启此服务，保留数据目录；客户端会重连。安装脚本为旧二进制保留 `.previous`，不清空凭据库。

## 参考

- [WebAuthn server implementation](https://docs.rs/webauthn-rs/0.5.5/webauthn_rs/)
- [JWT 验证约束](https://www.rfc-editor.org/rfc/rfc8725.html)
- [Certbot webroot 与续期](https://eff-certbot.readthedocs.io/en/stable/using.html)
