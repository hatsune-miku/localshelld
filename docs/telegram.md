# Telegram 授权

Telegram 与桌面弹窗是并列的授权入口，可以同时启用。请求设备只接纳一次结果：Telegram 完成后关闭 Windows/macOS 弹窗；桌面完成后，Telegram 消息显示“已经在其它端操作”并移除按钮。请求到期或取消也会移除按钮，网络中断时后台重试。

## 配置

1. 在“云同步”中连接云服务，再使用 BotFather 创建 bot。
2. 本机管理页进入“桌面授权 → Telegram 授权”，保存 Bot Token。
3. 选择“绑定 Telegram 账号”，打开生成的链接并选择 Start。
4. 页面显示已绑定后，可以发送测试消息。

绑定链接包含随机、15 分钟有效的一次性能力。仅接受与此链接匹配的私聊绑定，后续按钮与备注只接受绑定用户、绑定私聊和对应消息。不要把绑定链接交给其他人。

授权消息提供接受、拒绝、忽略、记住选择；回复消息可添加备注。忽略保持原来的 60 秒超时语义。消息显示请求设备、代理名称、权限范围、凭据名称和申请原因，不发送凭据值。

Telegram 授权仅在连接云服务后可用。token、启用状态和私聊绑定作为第二项配置共同同步，并在服务端和本机缓存中加密保存。它们不属于凭据导入/导出文件。只有云服务维持唯一的 Telegram 长轮询工作器，所有设备通过已认证的云会话接收各自请求结果；设备客户端不包含 Bot API 接收器。云服务故障不会回退到本机轮询，桌面弹窗仍可使用。

断开云服务后，Telegram 在该设备上不可配置、绑定或使用，保留的加密缓存不会启动轮询。一个 bot 只能交给一个云服务实例管理，不能同时用于另一独立部署或 webhook。同一数据目录有排他锁，第二个服务进程会拒绝启动；多个设备和管理请求共享同一个工作器。

需要代理时，在后端服务配置 `HTTPS_PROXY`。systemd 服务不会自动继承登录终端的代理变量，可以通过 `systemctl edit localshelld-sync.service` 添加：

```ini
[Service]
Environment="HTTPS_PROXY=http://127.0.0.1:7897"
Environment="HTTP_PROXY=http://127.0.0.1:7897"
Environment="NO_PROXY=localhost,127.0.0.1,::1"
```

确认本机代理监听端口后重启服务。代理只需部署在后端，设备不需要直连 Telegram。Bot Token 不出现在错误消息或日志中。移除配置只移除 localshelld 中的配置，不删除 Telegram 上的 bot。

## 命令行配置

也可在已连接云服务的设备上停止对应 daemon 后运行 `localshelld configure-telegram --user`，从标准输入提供一行 token，再打开命令输出的绑定链接。不要把 token 放到命令行参数中。此命令保留原配置、凭据与云登录，并占用 daemon 的监听端口，防止与运行中的 daemon 并发修改同步会话。完成后重新启动 daemon。

## 同步扩展

`GET /sync/v1/info` 的 `features` 包含 `telegram-v1`。新客户端发送 `X-Localshelld-Features: telegram-v1` 后，凭据快照额外包含 `telegram`；旧客户端仍收到原快照结构。旧版 `PUT /vault` 仅替换凭据，不删除 Telegram 配置。两类配置共享递增 revision 和冲突检查。

`POST /sync/v1/telegram` 接受 `{expected_revision, command}`，command 为 `status`、`configure`、`remove`、`bind_start`、`bind_poll` 或 `test`。配置变化提交后递增 revision 并通知所有设备。本机 `/v1/admin/telegram` 只接受人类管理会话，不接受执行令牌。

`POST /sync/v1/telegram/request` 提交 `{prompt, device}`；`/telegram/poll` 接受 `{id}`，最多等待 20 秒；`/telegram/finish` 接受 `{id, completion}`。请求属于创建它的云设备会话，poll 返回前再次检查会话是否已撤销。不存在可供 HTTP 调用者直接提交接受/拒绝的接口。结果先作为候选交给设备仲裁，再更新消息；迟到或重复回调不能再次授权。
