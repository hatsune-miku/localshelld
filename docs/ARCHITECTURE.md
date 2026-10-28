# 可行性分析与架构

## 结论

trustd 帮助用户将自己设备的执行身份与凭据授权给 AI Agent；常规 stdio 调用由程序保管 handle，模型不必搬运执行凭据。任务、执行身份和授权生命周期由用户决定，行为约定仅为尊重用户的个人隐私。

Windows 使用 PowerShell 7 (`pwsh`)，Linux/macOS 使用 `/bin/bash`（兼容 macOS Bash 3.2）。认证、令牌和协议层使用安全 Rust，共享持久会话抽象；平台差异集中在 shell 后端、服务生命周期、文件 ACL 和执行身份。服务适配分别为 Windows SCM、Linux systemd、macOS launchd。

## 组件

| 程序 | 角色 |
| --- | --- |
| `trustd` | 维护 CLI/向导，安装、更新、配置、卸载 |
| `trustd-system` | Windows LocalSystem daemon |
| `trustd-root` | macOS/Linux root daemon |
| `trustd-user` | 非提权用户 daemon、桌面授权 |
| `trustd-stdio` | 普通用户 MCP 子进程，保管本进程的授权会话 |

维护模块只编译进维护程序；daemon 入口只启动对应身份，不提供维护子命令。共享 Rust 库保留 `localshelld` 名称，数据、加密 AAD、配对与同步标识不随产品名变化。网页资源继续嵌入程序，不另起静态资源服务。

### stdio 边界

Agent 宿主通过 stdio 连接接入进程，接入进程使用现有 loopback HTTP API 调用 daemon。handle/token 存在接入进程内存中，stdio 输入 schema 和授权输出移除这些字段；工具参数中的手工 handle/token 被拒绝。显式 `secrets_get` 仍返回用户要求的明文，任意命令输出不自动脱敏。

一个进程一个会话，没有按代理名称或“最近授权”寻找会话的全局状态。生命周期变更串行，长命令不持有生命周期锁，销毁/取消可以并发终止执行。每个请求独立记录取消信号；取消授权不会简单丢弃 HTTP 响应，而会接收迟到的 handle 后清理。EOF 后拒绝新授权并尝试销毁；强杀、断电与 daemon 不可达不保证清理，用户仍可撤销签发端记录。

预授权 token 通过宿主环境配置而非模型参数进入；各进程必须单独分配 token。服务报告已有存活会话时拒绝接管，避免销毁其他调用者。宿主多聊天共享同一进程时也共享权限，这不是聊天级身份认证。HTTP MCP 的 transport session 仍只承担协议取消能力，不替代执行授权。

### 用户态与高权限 daemon

两个 daemon 使用独立可执行文件：Windows `trustd-system` 必须为 LocalSystem，Unix `trustd-root` 必须为 root；`trustd-user` 必须为非提权登录用户。不存在 `--allow-unelevated` 绕过参数。默认端口分别为 32119 和 32120，配置目录、Master/TOTP 和签发令牌相互独立。

MCP 入口推荐用户 daemon，工具名为 `secrets_list`、`secrets_get`、`enter_interactive`、`enter_preauthorized`、`run`、`destroy`、`revoke`。Windows `run.privilege_level` 必填，三选一：`current-user`、`as-administrator`、`nt-authority/system`；Linux/macOS 保持必填 boolean `sudo`。普通用户执行路由至用户 daemon，高权限执行须持有系统授权。每种身份拥有独立的 shell 状态，不复制已有 shell 的变量或目录，不自动回退。

Windows 的管理员后端使用配对用户进程的 linked elevated token，校验进程创建时间、SID、登录 session 及令牌属性，保证管理员与当前用户是同一个账户；不以 SYSTEM 修改环境变量冒充。通过配对加密桥取得用户环境和 cwd，保留用户 USERPROFILE、真实 HKCU 和用户 PATH。为重定向管道继承，在 broker 的非交互 session 内运行，不保证交互桌面或映射网络驱动器。PowerShell 仍使用 `-NoProfile`，不自动执行 profile 脚本。无 linked token 或 OS 权限不足时明确失败。SYSTEM 选项只允许真实 LocalSystem。

用户态令牌独立验证，系统 daemon 不在线仍可运行普通命令。用户提交系统 daemon 的令牌时，用户 daemon 先向签发端 enter，再创建用户 shell、独立 handle 和内存映射，只保留上游 handle；不会读取或转交 Master Password/TOTP。控制请求限制 5 秒，长轮询限制 20 秒，禁止代理和重定向，目标限定为 loopback SocketAddr，响应读取有大小上限。命令转发不默认限时、不重试。

上游的 validate/watch 接口验证 handle，不依赖执行锁；watch 至多挂起 15 秒，撤销或 shell 结束可提前唤醒。用户 daemon 在每次普通执行前验证上游会话，另持续 watch 授权生命周期，失联、撤销、到期或关闭时取消本地 shell。任何一端 shell 结束时整个双权限 handle 失效；用户主动销毁或关闭也请求销毁上游会话。此机制提供跨进程取消，不能保证机器/网络失效时即时收到通知；通信超时后失败关闭。

用户 daemon 与用户共享操作系统账户，高权限 daemon 通过用户签发的系统令牌接受委托。两种 daemon 的职责是选择执行身份并管理会话生命周期。

### 授权与显示分层

- `auth`：Master/TOTP、运行期执行令牌；`management`：10 分钟管理会话、WebAuthn 和凭据设置。
- `settings` / `storage`：原子、受 ACL 保护的配置写入，以及独立的 `config.authorization.json`。后者保存配对共享密钥、通行密钥公钥/元数据、用户 UUID、记住的选择及 key 范围、加密后的环境变量凭据；备份或迁移时需同时保留它、原配置和 seal key。
- `consent`：最多 8 个 pending 请求、60 秒截止时间、取消清理与进程内一次性决策回调；`server` 统一决定权限与创建会话。
- `desktop`：平台适配。Windows 的 `crates/native-dialog` 封装 Win32 FFI；应用主库继续禁止 unsafe。模型无关的窗口层只接收文字和完成回调，不知道 token、shell 或提权接口。Linux/macOS 保留 Tao/Wry 实现。
- `bridge`：显式配对后使用 AES-256-GCM 信封，绑定角色、方向、请求 ID，限制请求年龄并阻止重放。控制请求有短超时，授权请求遵循 60 秒截止时间，命令不默认限时、不重试。

Windows 使用模型无关的原生无模式对话框，`IsDialogMessageW` 负责键盘导航，W 系列控件处理 Unicode。GUI 线程只处理窗口、布局和事件；有界 Rust 队列与 `PostMessageW` 传递唤醒，停止事件不受队列满影响。GUI 不读取配置、不阻塞等待 HTTP、不创建 shell。DPI 切换时替换字体并重排，关闭 HWND 后释放 GDI 资源。原生窗口不需要 WebView2；Windows 构建不引入 Wry/Tao。

记忆接受不在工具结果中标记自动，且不谎称发生了本次点击；记忆拒绝明确标记自动。忽略和关闭是“不作决定”，由授权端等到原始截止时间。客户端断开或 UI 失败不产生授权。代理名称是自报信息，同名请求共享选择，不具备身份认证强度。

配对用户 daemon 是系统授权信任边界的一部分：控制该普通用户进程或其账户的主体可能模拟批准。原生窗口不是 UAC 安全桌面，也不是防本机恶意软件的可信输入通道。这里提供用户可控的本机委托，不宣称能够抵御同账户失陷。

未来内核能力应新增独立执行后端，复用授权结果与生命周期；不得把驱动逻辑塞进 UI、WebAuthn 或 bridge。签名和内核代码本次均未实现，决策依据见 [Windows 驱动签名调研](WINDOWS-DRIVER-SIGNING.md)。

## 关键取舍

### 命令执行

不自行解析或重写用户命令。Windows 启动 `pwsh -NoProfile -NonInteractive -EncodedCommand` bootstrap，命令通过 UTF-8→Base64 编码传输，由 `[ScriptBlock]::Create` 原生解析并在当前作用域执行。Unix 启动 `/bin/bash --noprofile --norc -c` bootstrap，移除 `BASH_ENV` 等启动变量；命令按字节编码传输（可打印 ASCII 直传、其余转义为八进制），由 Bash 内建 `printf -v` 无外部依赖地还原，再在父 shell 中 `eval`，不使用丢失状态的命令替换或子 shell。因变量无法承载 NUL，含 NUL 的命令在入口即被拒绝，不会静默截断。

Bash bootstrap 用 `builtin read`/`printf`/`eval` 读取协议，命令即便重定义同名函数也不影响读循环。协议输入 fd 3、stdout 帧 fd 4、stderr 帧 fd 5 与命令输入分离；命令内的 `exec >…` 不会静音后续命令。单次 `stdin` 保存在私有临时文件中供命令读取到 EOF，允许 UTF-8 字节中的 NUL；完成或中断后清理。PowerShell 保存独立的控制 reader，执行期间同时切换 Win32 标准输入 handle 与 `Console.In`，完成后恢复。

随机 144-bit marker 和控制字符将每个请求在 stdout/stderr 上的完成帧定界；并行读取两个管道，各自最多保存 `max_output_bytes` 字节，截断后继续排空。PowerShell 对象转为文本，error 流进入 stderr，其它逻辑流不单独建模。结果提供 stdout、stderr、真实 identity、cwd、duration_ms、退出/超时/会话状态，兼容 output 是 stdout 后接 stderr，不保证交错时间顺序。handle 是独立的 256-bit 随机秘密。

这比“每次调用启动一个 shell”多一些 framing 复杂度，但保留了变量、环境和当前目录，符合会话语义。比自行实现 shell grammar 可靠得多。命令本身拥有 root 权限，因此刻意伪造 framing 不构成额外权限提升。

每种身份的 shell 由独立 worker 持有进程和输入输出，队列串行执行命令；取消信号不依赖执行锁，因此无限时命令也可以被销毁、撤销或服务关闭中断。读循环同时监听 shell 退出，`exit`/`exec`/`set -e` 结束 shell 时返回部分输出与实际退出码，并结束整个 handle。普通命令非零退出码不结束 shell。

Windows `crates/native-process` 隔离进程相关 FFI，主库继续禁止 unsafe。创建 shell 时使用显式标准句柄继承列表，在 suspended 状态加入禁止 breakaway 的 kill-on-close Job Object 后才恢复执行；销毁时终止整个 Job，覆盖子孙进程。Unix 记录独立进程组，清理时先枚举仍可识别的后代、从叶子向上终止，再对原始进程组发 SIGKILL；已经 double-fork 并重新托管的 daemon 不保证回收。

HTTP 调用方断开不自动取消命令；执行与清理生命周期继续，避免后续串包和遗漏超时清理。显式执行超时或取消销毁整个 handle 的所有身份 shell，返回部分 stdout/stderr、`exit_code:null`、结束原因，不承诺保留状态。MCP initialize 签发随机 `Mcp-Session-Id`，run 的取消通知按该客户端 capability 与 requestId 定位，不能仅凭 requestId 跨客户端取消。服务关闭先拒绝新会话、取消已有会话，再等待 HTTP 请求退出。此内部生命周期不是后台任务系统；没有任务 ID、轮询或持续输入 API。

默认不设命令或会话运行时长上限，删除 `max_command_seconds` 配置；调用方仍可显式指定正整数 `timeout_seconds`。单 daemon 令牌有效期只对授权及后续调用做检查；双权限会话的 watch 会在高权限授权到期时取消两端 shell。旧 Unix 默认 `shell = "pwsh"` 在加载配置时迁移为 `/bin/bash`，旧自定义路径需手动修改。

### 短期令牌

令牌由 CSPRNG 生成，使用 22 位纯 ASCII 字母数字 Base62 编码，约有 131 bit 熵。原始令牌只在签发时返回一次，服务端仅在内存中保存 SHA-256 哈希、管理 ID、签发时间、到期时间和撤销状态。服务重启后内存记录消失，因此即使声明永久的令牌也会自然失效。

### 凭据存储

Master Password 只保存 Argon2id PHC verifier。TOTP 验证使用可恢复的共享 secret，以 AES-256-GCM 加密存储，seal key 单独保存并使用操作系统 ACL/文件权限管理访问。

环境变量凭据由 `secrets` 管理，key 必须为可移植的大写环境变量名，value 同样用 AES-256-GCM 加密后写入 `config.authorization.json`，复用独立 `seal.key`。管理页支持写入、更换、删除，读取接口只返回名称。配对后以桌面用户 daemon 为凭据库，两端后台经认证加密桥操作同一份数据；未配对时使用各自独立库，不自动迁移。

`enter.secret_keys` 选择本次需要的凭据，复用原有 token / 交互授权。用户同意后在 session 中保留不可变的环境快照，各身份 shell 创建时直接覆盖对应子进程环境，不在命令文本、参数或日志中插值。Windows 按不区分大小写的名称替换继承项；管理员后端先取得同用户 profile 环境，再应用所选凭据。惰性创建的用户/管理员 shell 也使用原快照，不重新读取已轮换的值。内部 `SessionEnvironment` 仅在配对加密桥上、验证有效 handle 后返回快照，不作为公开 secret 工具。

记忆允许另外记录曾确认的 key 范围，新增 key 必须再次显示授权窗口；没有新增 token 类别。`secrets_list` 只列名称，`secrets_get` 通过存活 handle 显式读取该会话已授权 key 的明文快照，与 HTTP `POST /v1/secrets/get` 共用实现；关联会话在读取前再次验证系统授权，失联时不返回缓存。列表和 enter 响应不含 value，但注入不是针对 shell 本身的保密边界：获授权的命令仍可读取并输出环境。删除后台凭据不清理已有进程环境，终止已有委托应销毁会话或撤销其 token。

### 本机传输

HTTP/MCP 使用本机 loopback，配置验证限制 bind 和 daemon 间通信目标为 loopback 地址。项目不提供远程监听或公用服务器模式。

### 整套安装与显式更新

`update` 仍是维护 CLI 的显式操作，不在 daemon 中提供或后台自动运行。固定 GitHub 仓库、版本选择、HTTPS 和 SHA-256 校验保留，发布包必须同时包含维护、用户、stdio 与对应平台高权限程序。每个程序的版本都要匹配选定 Release；不接受旧单文件附件。

普通用户更新下载并校验完整套件，再启动该套件的维护向导；`--download-only` 只输出已校验目录，`--check` 只查询。启动向导不是更新完成，服务切换和系统授权由维护流程统一处理。

维护计划绑定所有程序的名称和 SHA-256，版本目录使用整套摘要。只允许固定平台角色，不接受任意路径或额外文件。暂存、提权后与安装后分别核验，源程序不覆盖，已有摘要目录内容不符即失败。旧配置、授权状态和 seal key 备份后才切换服务；系统安装失败尝试恢复旧注册，用户端启动失败明确报告并保留修复入口。配置、系统服务标签与协议身份保留 `localshelld`，程序与界面显示 trustd。

系统保险库本轮不接入，seal key 仍受本机文件权限保护；没有新的 Keychain/Credential Manager 解锁依赖。用户管理页保留明文导入/导出，std​io 不改变这项用户能力。

## 需求覆盖

| 原始需求 | 实现 |
|---|---|
| Windows Administrator / SYSTEM / Unix root | 同用户 linked elevated token / SCM LocalSystem / systemd 与 launchd root |
| 普通用户执行 | 独立用户 daemon、非高权限身份检查、用户登录自启动 |
| 显式选择执行权限 | Windows 三态 privilege_level；Unix boolean sudo；独立状态、无身份回退 |
| 每次运行令牌自然失效 | opaque token 元数据只保存在进程内存中 |
| 用户亲自签发 token | 本地 UI + Argon2id Master / TOTP |
| 默认 24h、可永久 | issue API 和 UI presets |
| 一个 token 一个 session | 双向 token-id/handle map，明确 reused 响应 |
| handle 是强密码 | CSPRNG 256 bit base64url |
| 平台原生 shell 语义 | 持久化 `pwsh` 或 Bash 原生解析，平台集成测试 |
| 销毁 session/token | 立即移除映射并清理所有关联 shell 进程树；撤销级联 |
| HTTP + MCP | Axum JSON API + MCP JSON-RPC Streamable HTTP |
| MCP shell 识别 | 平台相关说明与 enter 的 shell/platform 字段 |
| 多平台二进制 CI | Windows/Linux/macOS ARM64 与 x86_64 matrix + release artifacts |

## 运行特性

- 输出是分别限额的 stdout/stderr 聚合响应，支持单次 stdin + EOF，不提供实时流、PTY 或后续输入。命令可运行至完成，客户端有独立超时；长任务管理由 harness 负责，不新增任务系统。
- 各平台服务注册分别指向专用 daemon；维护向导把整套程序放入摘要版本目录，再切换注册，不原地覆盖正在运行的程序。macOS 使用 `/Library/LaunchDaemons/dev.localshelld.plist`，`bootstrap system` 安装，`bootout` 卸载；配置和 seal key 保留。
- Windows 使用 Job Object 清理进程树，Unix 清理可追踪后代和原始进程组。主动彻底脱离的 Unix daemon 不在回收保证内，这不是容器或作业调度器。刻意改写 `__localshelld_` 内部变量只会破坏自身会话，不构成额外提权。
- 日志不记录命令和凭据，用户的任务内容保留在会话中。
