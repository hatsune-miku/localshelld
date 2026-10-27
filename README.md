# localshelld

> 你的设备，由你掌控。localshelld 帮助你将执行权限临时授权给 AI Agent。

localshelld 是面向有经验的设备所有者和维护人员个人自用的开源项目。项目的出发点是：用户自然拥有对自己合法所有设备的终极掌控权，也可以决定将其中的执行权限临时交给 AI Agent。用户决定任务、执行身份、授权时长与何时收回授权，工具负责实现这份委托。

项目由使用者自行部署和维护，不面向普通终端用户分发，也不提供公用服务器或托管授权服务。代码、构建和版本流程开放，供有经验的维护人员在自己的设备上使用。

唯一的行为约定：**尊重用户的个人隐私。**

## 为什么给 AI 使用

当 Agent 的运行环境或沙箱妨碍用户完成自己的任务时，localshelld 提供现成的本机执行通道。Windows 使用 `run.privilege_level` 显式选择当前用户、当前用户管理员或 SYSTEM；Linux/macOS 继续使用 boolean `sudo`。长任务调度属于 harness，本项目不提供另一套后台任务系统。

整个过程由用户掌控：

- **临时委托**：用户可签发令牌，或直接在桌面弹窗中批准 Agent 的请求；授权可随时撤销。
- **直接执行**：Agent 使用 localshelld 完成用户交代的工作，无需反复处理沙箱权限限制。
- **原生脚本体验**：Windows 命令交给持久化 `pwsh`，Linux/macOS 命令交给持久化 `/bin/bash`，由原生 shell 解析。
- **少打扰**：一个 token 自动复用一个会话，变量、工作目录和环境可以跨命令保持，不必每执行一步都重新授权。
- **随时收回**：撤销令牌会结束关联会话；daemon 重启后，该实例签发的令牌失效。

典型工作流只有四步：

```text
Agent 判断任务需要本机执行能力
    → 通过 Ask 请求 localshelld 令牌
    → 用户在本地页面签发并粘贴令牌
    → Agent 进入会话、完成任务并销毁会话
```

一次授权对应一段任务。签发和管理在本地页面完成，执行过程中无需逐条命令重复审批。

## 核心能力

- 同一个 Rust 二进制支持两种 daemon：系统 daemon 要求 Administrator/root；`--user` daemon 拒绝以 root/管理员身份运行。
- 令牌是 22 位纯字母数字随机串，约 131 bit 熵；服务端只在内存中保存 SHA-256 哈希和授权元数据。
- Master Password 使用 Argon2id 保存为不可逆 verifier；TOTP 使用 RFC 6238 SHA-1/6 位/30 秒配置，兼容 Proton Authenticator。
- TOTP secret 以 AES-256-GCM 加密，配置和 seal key 独立保存并受权限保护：标准系统目录仅允许 SYSTEM/Administrators 或 root；用户配置仅允许当前账户及系统管理员。
- 默认令牌有效期 24 小时，支持自定义有效期与“永久”（仍随服务重启失效）。
- 每个 daemon 对同一令牌复用一个 handle；双权限 handle 关联两套独立 shell。handle 为 256 bit CSPRNG 随机值。
- 命令直接交给平台原生 shell 解析和执行，支持管道、通配符、多行、Unicode、环境变量、函数/变量/目录跨调用保持和原生命令退出码。
- 会话没有独立的运行时间上限，命令默认无限时；调用方可选传 `timeout_seconds`，不设服务端最大值。令牌的授权有效期规则不变。
- 撤销令牌会同时终止其全部会话；命令超时或执行器失联会销毁会话。
- HTTP 和管理 UI 强制只绑定 loopback；敏感值只放 JSON body，不放 URL。

## 前置条件

- Windows 10/11：安装 [PowerShell 7 (`pwsh`)](https://learn.microsoft.com/powershell/scripting/install/installing-powershell) 的系统级版本，向导使用 Program Files 下的 `PowerShell/7/pwsh.exe`
- Linux：使用 `/bin/bash`，自动安装服务需要 systemd
- macOS：使用系统 `/bin/bash`（兼容 Bash 3.2）和 launchd；预编译发布包仅提供 Apple Silicon，Intel 不再纳入 CI 和发布流程
- 从源码构建需要 Rust stable、Node.js 22 与 pnpm 10.14；先构建 Web 资源，再构建 Rust
- Linux 图形授权需要登录桌面和 WebKitGTK 4.1 / GTK 3；Debian/Ubuntu 构建依赖为 `libwebkit2gtk-4.1-dev libgtk-3-dev libssl-dev pkg-config`
- Windows 授权窗口使用 Win32 原生控件，不依赖 WebView2；编译 WebAuthn 所需的 OpenSSL 时需要 Perl/NASM
- 安装向导使用本机浏览器；Windows 使用 UAC，macOS 使用系统管理员授权，Linux 使用 polkit/pkexec。向导本身以普通登录用户身份启动

## 图形化安装与维护

以普通登录用户运行 `localshelld maintenance` 或 `localshelld setup`；无参数启动也会打开向导。Windows 可以直接双击程序。无需另装 WebView 运行时，页面资源嵌入同一个二进制。

向导提供环境检查、初始化、安装或修复、旧安装迁移及服务卸载。安装会请求系统授权并短暂重启服务；两个 daemon 的健康检查和用户服务身份核验通过后才显示完成。首次安装仍需到两个管理页面手动配对，已有密码、TOTP、配对、通行密钥、凭据和授权记忆不重置。

| 平台    | 程序目录                    | 系统配置                                                      |
| ------- | --------------------------- | ------------------------------------------------------------- |
| Windows | `Program Files/localshelld` | `ProgramData/localshelld/system/config.toml`                  |
| macOS   | `/Library/localshelld`      | `/Library/Application Support/localshelld/system/config.toml` |
| Linux   | `/opt/localshelld`          | `/etc/localshelld/system/config.toml`                         |

Windows 路径由系统 Known Folder API 获取，不采用环境变量覆盖。实际二进制放在程序目录的 `versions/<SHA-256>/` 中；程序目录允许普通用户读取和执行，但不可写。系统配置及备份仅对系统管理员开放。用户配置继续留在当前用户的独立目录。安装不修改 PATH。

迁移读取现有系统服务的注册路径，保留旧程序和配置，并把原配置、授权状态及 seal key 备份到系统数据目录的 `backups/`。迁移会核对状态和密钥内容未改变，仅把系统 shell 固定为系统级绝对路径；旧系统配置副本也会收紧权限。系统安装失败会尝试恢复原服务注册。用户自启动失败会明确报告，可重新执行修复。

安装成功后，Windows 开始菜单、macOS 用户 `Applications` 或 Linux 应用菜单会增加维护入口。卸载仅停止并注销服务，保留程序、配置和备份。`maintenance --no-open` 可只输出本机向导地址；链接是本次进程的临时访问凭据，不应分享。退出向导或 30 分钟后停止监听。

macOS 仍使用 launchd 和 Bash 3.2；授权弹窗走 Tao/Wry 的系统 WebView，Windows 专用 Win32 控件位于独立编译分支。Apple Silicon 仍在 CI 和发布矩阵内，Windows 本地测试不代表 macOS 图形授权已通过实机验证。

## 构建与初始化

```powershell
pnpm --dir web install --frozen-lockfile
pnpm --dir web build
cargo build --release
./target/release/localshelld maintenance
```

也可使用 CLI `init` 初始化；系统默认配置已改为上表中的机器目录，写入需要管理员/root 权限。初始化不设置字符组合规则，也接受空密码；空格按原样保留。终端输入使用普通文本回显，网页输入也直接显示文本。密码仍只保存 Argon2id verifier。`--password-stdin` 从标准输入读取一行并省略重复确认，输入流提前结束则报错。CLI `init --totp` 会显示 `otpauth://` URI 和手动 secret，并要求输入当前动态码确认绑定；配置确认成功前不会落盘。图形向导首次初始化后可在管理页面添加 TOTP 和通行密钥。

开发运行（非管理员，仅用于本地验证）：

```powershell
./target/release/localshelld init --config ./localshelld.local.toml
./target/release/localshelld serve --allow-unelevated --config ./localshelld.local.toml
```

默认一次安装并立即启动系统 daemon 和当前登录用户的 daemon，随后手动建立互信。Windows 请使用同一登录用户的 Administrator 终端；Linux/macOS 请从该用户的登录会话通过 `sudo` 执行：

```powershell
./target/release/localshelld install
```

Windows 注册为 `localshelld` 自启动服务并以 LocalSystem 运行；Linux 写入并启用 `localshelld.service`；macOS 写入 `/Library/LaunchDaemons/dev.localshelld.plist`，通过 launchd 以 root 自动运行。低层 CLI `install` 仍直接注册当前二进制路径，不负责搬迁和收紧自定义目录；需要标准布局时使用 `maintenance`。若初始化和安装使用不同账户，请在两条命令中都传入同一个绝对配置路径：`--config <path>`。二进制应放在稳定路径中，安装后不要移动它。

安装器同时注册当前用户的登录自启动项，并立即启动用户 daemon（默认 `32120`）。若用户配置不存在，安装阶段就提示设置其独立 Master Password；也可以提前用 `init --user --totp` 初始化。已有用户配置、验证方式、配对密钥和记忆不会被覆盖，不会从系统端复制凭据，也不会自动建立互信。使用非默认用户配置时传入 `--user-config <path>`；已有配置的 `privileged_daemon` 必须指向这次安装的系统端。

两个入口通过健康检查后，安装器才报告安装完成并显示手动配对说明。安装或启动失败会明确报错，不会等到第一次命令执行才启动用户端。已经安装系统端的旧用户可以在原二进制路径再次运行 `install` 补齐用户端：匹配的现有服务/任务会被复用，不重启运行中的进程；路径或配置不一致时保留原注册并报错。确实只需要系统服务的无头主机可显式使用 `install --system-only`，不会猜测登录用户或把用户端以 root/LocalSystem 运行。

macOS 推荐通过普通用户启动向导；高级 CLI 安装示例（先把二进制放到管理员管理的 `/usr/local/bin/localshelld`）：

```bash
sudo /usr/local/bin/localshelld init --totp
sudo /usr/local/bin/localshelld install
# 查看服务状态
sudo launchctl print system/dev.localshelld
```

`uninstall` 默认停止并注销系统服务和当前登录用户的自启动项，与默认安装行为对应。需要 Administrator/root 权限；Linux/macOS 应从目标用户的登录会话通过 `sudo` 执行。配置、`seal.key`、配对和 Passkey 数据均保留：

```powershell
./target/release/localshelld uninstall
# 仅卸载系统服务，保留用户端（也适用于无用户桌面的主机）
./target/release/localshelld uninstall --system-only
```

只移除当前用户端可用 `localshelld uninstall --user`。Windows 允许以同一账户的普通或管理员令牌执行此卸载；由管理员创建的计划任务可能需要 `sudo localshelld uninstall --user`。SYSTEM/Session 0 不能代替登录用户卸载用户任务。运行、初始化和安装用户 daemon 的非提权限制不变。卸载仅管理已注册的服务/自启动，不会按端口查杀手动启动的其他进程。

系统 daemon 默认监听 `127.0.0.1:32119`，在此签发的令牌允许高权限操作：

- 管理 UI：`http://127.0.0.1:32119/`
- 系统 MCP：`http://127.0.0.1:32119/mcp`（配对用户 daemon 后，也支持当前用户执行）
- 健康检查：`http://127.0.0.1:32119/health`

## 用户态 daemon

完整 `install` 已包含用户 daemon。需要独立安装或管理用户端时，在普通用户终端执行以下命令（`install --user` 遇到未初始化的配置也会当场引导初始化）：

```powershell
localshelld init --user --totp
localshelld install --user
# 也可前台运行，便于调试
# localshelld serve --user
# 停止并移除当前用户的自启动项
# localshelld uninstall --user
```

用户态配置使用独立的 `localshelld-user` 平台配置目录，默认监听 `127.0.0.1:32120`；所有命令仍可传 `--config`。Linux 安装为 `systemctl --user` 的 `localshelld-user.service`；macOS 安装为当前 GUI 登录会话的 `dev.localshelld.user` LaunchAgent；Windows 安装为当前用户 SID 对应的登录计划任务，使用交互用户令牌和最低权限。Windows 用户 daemon 启动后会释放只属于自己的控制台，不留下命令行窗口；从现有终端手动运行时保留共享终端和输出，原生授权弹窗不受影响。它随用户登录启动，不自动启用 Linux linger。多用户同时运行时需为各自的用户 daemon 配置不同端口。

Agent 连接 `http://127.0.0.1:32120/mcp`：

- 在用户页面 `http://127.0.0.1:32120/` 签发的令牌，只允许当前用户执行。系统 daemon 未运行时也可使用。
- 在系统页面 `http://127.0.0.1:32119/` 签发的令牌允许当前可用的高权限身份；进入响应返回 `privilege_levels` 和 `identities`。Linux/macOS 另外保留 `sudo_available`。
- 每种身份的变量、函数、工作目录和环境分别保持，不在已有 shell 之间复制状态。执行身份必填，不自动提权、不换身份重试。

### Windows 执行身份

| `privilege_level`     | 实际身份与环境                                                       |
| --------------------- | -------------------------------------------------------------------- |
| `current-user`        | 用户 daemon 的非提权账户和环境                                       |
| `as-administrator`    | 同一用户的真实管理员令牌，保留该用户的 USERPROFILE、HKCU 和用户 PATH |
| `nt-authority/system` | LocalSystem；配置目录、HKCU、凭据和环境不属于登录用户                |

`as-administrator` 要求已配对的普通用户 daemon 正在运行，且该用户具有可用的 linked administrator token。系统端通过配对加密通道获取用户进程上下文，校验 PID 创建时间、SID 和令牌后启动管理员 shell；没有令牌或权限不足时明确失败，不使用其他管理员账户或 SYSTEM 顶替。运行于非交互环境，不承诺映射网络驱动器或交互桌面可用。保留操作系统用户 profile 不等于执行 PowerShell 的 `$PROFILE` 脚本：仍使用 `-NoProfile`。

Windows run 不再接受 `sudo`，也不接受 `root`。手动以普通管理员运行的系统 daemon 不是 SYSTEM，不能执行 `nt-authority/system`；安装为 LocalSystem 服务才提供该身份。Linux/macOS 保持 `sudo:false` 当前用户、`sudo:true` root，不接受 `privilege_level`。升级时两端 daemon 应使用相同版本。

- 销毁双权限会话会终止两个 shell。高权限令牌撤销、到期、系统 daemon 关闭或通信失败会关闭其关联的用户 shell；正在运行的命令也会取消。用户态独立令牌不受系统 daemon 重启影响。

用户 daemon 向系统 daemon 传递用户交给它的令牌与命令。Master Password 和 TOTP 用于各自本地页面的签发与管理；执行接口使用 token 和 handle。两种身份的可访问资源由操作系统账户权限决定。

## Web 验证方式与桌面授权

管理后台使用 Vite、React 和 CakeUI 构建，资源嵌入同一二进制，无需额外 Web 服务或 CDN。认证方式默认选择通行密钥；尚未注册时可切换到 Master Password 或 TOTP。应用不覆盖组件库的字体设置，密码输入保持普通文本并允许空密码。原有“复制授权文本”行为保留，也可单独复制令牌。

在“验证方式”中可以修改 Master Password，绑定/替换/移除 TOTP，添加或移除通行密钥。TOTP 必须输入当前动态码完成绑定；通行密钥由浏览器/系统认证器验证，私钥不交给 daemon。通行密钥注册和登录请使用 `http://localhost:32119/` 或 `http://localhost:32120/`，而不是 IP 地址；浏览器需支持 WebAuthn，系统与用户入口分别注册。管理登录仅在页面内存中保留，10 分钟后失效；改密码会清除全部管理登录，不会自动撤销已有执行令牌。

完整安装结束时两个 daemon 都已运行；使用 Agent 之前，应在管理页完成一次显式配对，使系统端的当前用户执行和高权限交互授权可用：

1. 登录 `http://localhost:32120/`，在“桌面授权”中生成配对码。
2. 登录 `http://localhost:32119/`，填写用户 daemon 地址和配对码，完成配对。
3. Agent 可继续连接任一 MCP 入口。系统端的普通命令会交给配对的用户进程，绝不在 root/管理员 shell 中假装降权执行。

配对码是两个 daemon 之间的共享密钥，不是交给 Agent 的访问令牌。仅与自己控制的用户进程配对。未配对时，系统端的普通命令会返回可操作的配置错误；直接使用用户入口及其令牌执行当前用户命令不需要配对。更改端口后需更新配置并重新配对。

`enter_interactive` 需要 `agent_name`、`request_reason` 和 `confirm_text: "OK"`，不需要传 token。已有用户 daemon 负责显示窗口，不新增 GUI daemon：Windows 为原生 Win32 对话框；Linux/macOS 使用系统 WebView。后者可能有 WebView 运行时子进程，命令执行也会创建 shell/命令子进程，不能把“两个 daemon”理解成整个进程树永远只有两个进程。

窗口原样显示缘由，包含接受、拒绝、忽略、记住选择及附加说明。忽略或关闭窗口不会立即拒绝，而是等到原请求的 60,000ms 截止时间；附加说明通过 `btw` 返回。接受后返回 handle，内部创建的 24 小时授权记录可在签发端令牌列表中撤销，不返回原始 token。记忆按 `agent_name` 精确匹配，持久化到磁盘；名称是代理自报信息，不是身份认证。自动接受的结果不标记为自动，也不声称本次有人点击；自动拒绝明确返回 `automatic: true`。

配对后的交互请求由系统端统一管理记忆；在系统管理页点击“清除所有记忆”即可重置这些选择。未配对用户入口的普通权限记忆由用户管理页清除。清除记忆不会撤销已经建立的会话。

Windows 对话框由系统处理 Tab/Shift+Tab、回车、Esc 和 Alt 快捷键。默认按钮为“忽略”，附加说明内回车用于换行。窗口按显示器 DPI 调整控件和系统消息字体，并遵循系统字体平滑设置。后台 IO、认证和 IPC 不运行在 GUI 线程。`serve --user --headless` 可禁用弹窗；没有桌面或没有可用配对端时会明确报错，不自动批准。

## 从旧名称迁移

本项目已从 SudoServer 更名为 `localshelld`。新版本使用新的服务名和默认配置目录，不会自动接管旧服务。已有安装请先使用旧二进制执行 `sudoserver uninstall`，再用新二进制执行 `localshelld install --config <原配置文件的绝对路径>`；保留原配置文件及同目录的 `seal.key`，即可继续使用原 Master Password 和 TOTP，无需重新 `init`。如需迁移配置目录，请将这两个文件一起迁移，并保留原有访问权限。卸载/重启会结束会话，原令牌需要重新签发。

MCP 配置中的服务键名改为 `localshelld`，推荐用户态端口 `32120`。当前工具列表使用下文的六个短名称；之前的 `localshelld_*` 长名称暂保留调用兼容，但不重复列出。Windows 客户端须同步从 `sudo` 迁移到 `privilege_level`，仅兼容旧工具名、不兼容旧 Windows 参数。构建和会话环境变量继续使用 `LOCALSHELLD_*` 前缀。

## 命令行自更新

```powershell
localshelld update --check                 # 只查询 GitHub，不需要提权
localshelld update                         # 最新稳定版
localshelld update --prerelease            # 包含 RC，仍按语义版本选择最新版
localshelld update --tag v0.1.0-rc.8.1      # 指定标签（示例）
# 确认需要降级时，同时指定 --tag 和 --allow-downgrade
```

更新来源固定为 `hatsune-miku/localshelld` 的 GitHub Releases，不需要 GitHub 登录。更新器不预设 Administrator/root 门槛；写入程序目录或控制已安装的系统服务需要额外权限时，由操作系统报错，再在 Administrator/root 终端重试。`--check` 不下载二进制、不修改文件、不停止服务。稳定通道同时排除 prerelease 标记和带预发布后缀的标签；即使 RC 在 GitHub 上标为正式 Release / Latest，版本号仍是 `-rc.*`，命令行更新仍需使用 `--prerelease`。

更新器自动读取 `HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY`，以及 Windows/macOS 当前用户的系统代理设置；支持 `NO_PROXY` 排除项。`HTTPS_PROXY` / `HTTP_PROXY` 可显式覆盖对应协议的系统代理，不需要关闭 TLS 校验。Windows/macOS 的 PAC 脚本不属于本功能支持范围，必要时指定静态 HTTP 代理。临时排障可在当前 PowerShell 中设置 `$env:HTTPS_PROXY = 'http://127.0.0.1:<代理端口>'`，再运行 `localshelld update --check --prerelease`。本机 daemon 通信与更新后的健康检查始终绕过代理。

更新前先完成下载、SHA-256 校验、平台匹配和二进制版本检查，再备份旧程序并更新。已安装的系统服务只在原本运行时停止和恢复；不会改写服务注册、配置、Master Password 或 `seal.key`。重启会中断会话并使全部运行期 token 失效。新版本服务状态与 `/health` 版本检查失败时，自动恢复旧二进制并尝试恢复原服务；回滚本身失败会保留备份并明确报错。

更新器管理的是系统服务，不会跨用户会话控制用户 daemon。若两种 daemon 共用同一个二进制，更新前请执行 `localshelld uninstall --user`（Windows 的管理员创建任务可能需要同一用户提权执行），更新后在普通用户终端执行 `localshelld install --user`。Windows 上仍运行的用户 daemon 可能占用二进制；Unix 上未重启的进程仍运行旧版本。

- Linux/macOS：在安装目录所在文件系统暂存完整文件，然后用 `rename` 原子替换路径，并同步目录。已运行进程继续使用旧文件，重启后使用新文件；这不是进程热升级，也不保证突然断电时整套服务操作具有事务性。
- Windows：把替换交给隐藏的本地助手，等待原 CLI 退出后操作。命令输出助手日志路径；只有日志中的 `SUCCESS` 才表示完成，启动助手本身不代表更新成功。更新失败详情也写在该日志中。
- 每次更新的 `.localshelld-update-*` 恢复目录保留 `previous` 旧二进制；Windows 还保留助手和日志。确认新版本稳定后，可在没有更新进行时手动清理对应目录。不会自动覆盖已有备份。
- 更新直接替换当前二进制，不因目录位置、所有者或普通用户可写而拒绝；也不自动修改安装目录的 ACL、所有者或服务注册路径。目录是否可信由设备所有者决定。空间不足、实际访问被拒绝、文件被占用等情况按操作系统的结果报错，仍保留校验、并发锁、备份与失败回滚。
- 自更新在独立终端中运行。`LOCALSHELLD_SESSION` 标记用于识别 daemon 创建的会话，该环境中支持 `--check`，实际更新在会话之外执行。手工 `serve` 前台实例需要自行停止/重启；自定义 systemd drop-in、非标准服务命令或处于过渡状态的服务采用手动更新。

正式版和 RC 统一提供 Windows ZIP、Linux/macOS tar.gz 与 `SHA256SUMS`。CI 把完整 Release tag 和 commit 写入二进制，`--version`、MCP 初始化和 `/health` 使用同一版本；本地构建默认显示 `<Cargo版本>-dev`。早期 RC 没有嵌入完整标签，版本检查会拒绝安装它们；首次使用自更新需要先手动安装包含本功能的新 Release。

更新使用 HTTPS、固定仓库与 SHA-256 校验，二进制和校验值均来自该仓库的 Release，目前未配置独立发布签名。网络、API 限流、校验或版本检查失败时，在停服前结束更新。

## 接入 AI Agent

将 Agent 的 MCP 客户端连接到：

```text
http://127.0.0.1:32120/mcp
```

以常见的 MCP 配置形式表示：

```json
{
  "mcpServers": {
    "localshelld": {
      "url": "http://127.0.0.1:32120/mcp"
    }
  }
}
```

连接成功后应看到 `secrets_list`、`enter_interactive`、`enter_preauthorized`、`run`、`destroy`、`revoke` 六个工具。两个 enter 分别表示桌面交互授权和预授权 token 进入；短名称保留了两种语义，并给客户端拼接命名空间留出空间。请刷新客户端工具列表；更早的 `localshelld_enter` 不恢复。

MCP 工具说明会明确当前平台和 shell。Agent 应按 `enter_preauthorized` 返回的 `shell`、`platform` 使用对应语法；Windows PowerShell 脚本不能直接用于 Linux/macOS Bash 会话。

Windows 当前用户执行：`run({"handle":"…","command":"whoami","privilege_level":"current-user"})`。保留用户环境的管理员执行选择 `as-administrator`；SYSTEM 选择 `nt-authority/system`。Linux/macOS 使用 `run({"handle":"…","command":"id","sudo":false})` 或 `sudo:true`。

可选 `stdin` 为单次 UTF-8 输入，写完即 EOF；省略时立即 EOF。不支持后续输入、PTY、后台任务或轮询接口。响应分开提供 `stdout`/`stderr`、实际执行 `identity`、`duration_ms`、`cwd` 和会话结束原因；不能仅凭 stderr 有文本判断失败。

客户端断连或自身超时不会自动停止命令。显式 `timeout_seconds` 超时会终止整个 handle 的所有 shell/进程树，并返回部分输出、`timed_out:true`、`session_alive:false`。支持 `run` 的 MCP `notifications/cancelled`，客户端须回传初始化响应的 `Mcp-Session-Id`，取消同样销毁整个 handle。普通命令非零退出码不会结束会话。

## API 概览

### 凭据注入

后台的“凭据”页面管理 key/value；Key 必须大写且直接作为环境变量名，例如 `JENKINS_CI_TOKEN`。值加密保存，列表及工具结果不返回已存的值。已配对时两个后台共用用户 daemon 的凭据库；未配对时使用各自的库。

输入值直接显示文本。管理页支持导出和导入版本化 JSON 凭据文件；文件包含明文值，由用户自行保密，导入会替换全部凭据。执行 token 和 MCP 不能调用导出接口。

“存储方式”默认使用本机离线库，也可连接自托管后端，通过通行密钥授权设备并同步凭据。连接后以云端为准：修改整库提交，云端确认后覆盖本机，版本冲突拒绝覆盖；在线设备通过实时长轮询更新。断网时不接受凭据操作或为新会话读取旧缓存，用户可以明确切换回离线模式。连接前应先导出本机数据，新服务的空云端库也会覆盖本机。

后端、开放协议、凭据文件格式及部署方式见 [docs/SYNC.md](docs/SYNC.md)。后端独立为 `localshelld-sync-server`，不提供 shell 执行接口。

Agent 可匿名调用 `secrets_list({})`，再把需要的 key 交给任一 enter：

```json
{
  "agent_name": "Example Agent",
  "request_reason": "查询用户指定的 Jenkins 构建。",
  "confirm_text": "OK",
  "secret_keys": ["JENKINS_CI_TOKEN"]
}
```

授权窗口显示请求的 key 列表。预授权仍使用原执行 token，无需单独的凭据令牌。后续 shell 命令直接引用 `$env:JENKINS_CI_TOKEN`（PowerShell）或 `$JENKINS_CI_TOKEN`（Bash）；同一会话各执行身份的 shell 均自动注入所选凭据。新增 key 不会沿用尚未覆盖它的记忆允许。

会话保留 enter 时的快照。更换或删除后台值不改变已有会话；更改存活会话的 key 选择须先销毁再进入。模型正常使用不需接收明文，但 shell 仍能打印环境变量，不能将此机制视为对获授权命令的防泄露隔离。

### 路径

本机写操作使用 `POST application/json`，避免把 token 或 handle 写入访问日志。完整示例见 [docs/API.md](docs/API.md)。

| 路径                      | 请求核心字段                                                               | 用途                 |
| ------------------------- | -------------------------------------------------------------------------- | -------------------- |
| `/v1/secrets/keys`        | 无，GET                                                                    | 列出凭据 key         |
| `/v1/sessions/enter`      | `token`、可选 `secret_keys`                                                | 建立或复用令牌的会话 |
| `/v1/commands/run`        | `handle`, `command`, Windows `privilege_level` / Unix `sudo`，可选 `stdin` | 显式选择执行身份     |
| `/v1/sessions/destroy`    | `handle`                                                                   | 终止会话             |
| `/v1/tokens/revoke`       | `token`                                                                    | 撤销令牌并终止会话   |
| `/v1/admin/tokens/issue`  | `credential`, duration                                                     | 签发令牌             |
| `/v1/admin/tokens/list`   | `credential`                                                               | 列出当前运行期元数据 |
| `/v1/admin/tokens/revoke` | `credential`, `id`                                                         | 由用户撤销指定令牌   |

## 配置

默认配置由平台配置目录决定。`init`、`serve`、`install` 可显式传 `--config`；`update` 从已安装服务的注册信息读取配置路径。主要字段：

```toml
bind = "127.0.0.1:32120" # 用户 daemon；系统 daemon 默认 32119
privileged_daemon = "127.0.0.1:32119" # 仅用户 daemon 使用
shell = "pwsh" # Windows；Linux/macOS 默认为 "/bin/bash"
max_output_bytes = 8388608
```

`shell` 可使用本平台 shell 的完整路径，但不支持跨 shell 切换。旧配置中的 `max_command_seconds` 被忽略，新配置不再生成该字段。Linux/macOS 的旧默认 `shell = "pwsh"` 会在加载时迁移为 `/bin/bash` 并记录提示，不改写原文件；旧自定义 PowerShell 路径需手动改为 Bash 路径。

HTTP 与 MCP 均为本机 loopback 接口，配置不支持远程监听地址。

## 授权与运行方式

- 正确提供 Master Password/TOTP 的主体视为用户本人；token/handle 表达用户授予的运行期权限。
- 管理凭据仅在验证时使用；日志不记录凭据和命令内容。
- PowerShell 使用 `-NoProfile -NonInteractive`，Bash 使用 `--noprofile --norc`。普通用户/SYSTEM/root 使用相应 daemon 身份；Windows 管理员使用配对用户的 elevated token 和用户环境。各 shell 不自动加载 profile 脚本。
- 响应在命令完成后返回，不是实时流；stdout/stderr 各最多保留 `max_output_bytes`，截断后仍排空。Windows 在执行前把 shell 纳入 kill-on-close Job Object，结束时清理整棵子进程树；Unix 清理仍可识别的后代进程及原始进程组。主动双重 fork、脱离进程树并重新托管的 Unix daemon 不保证回收；这不是容器或任务调度器。
- 会话非交互式，但支持单次 `stdin` 文本和 EOF。`exit`、`exec` 或 `set -e` 导致 shell 退出时保留输出与实际退出码，置 `session_ended=true`；超时/取消时 `exit_code:null`，不伪造命令退出码。NUL 命令被拒绝，stdin 可含 NUL。单 daemon 令牌过期阻止后续调用；跨 daemon 会话到期会终止关联 shell。

更详细的可行性与设计取舍见 [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)。

Windows 内核能力本次只做调研，不包含驱动代码、驱动安装或系统安全设置变更。签名路径、账号门槛和成本见 [docs/WINDOWS-DRIVER-SIGNING.md](docs/WINDOWS-DRIVER-SIGNING.md)。

## 验证

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
pnpm --dir web format:check
pnpm --dir web test
pnpm --dir web exec playwright install chromium
pnpm --dir web test:e2e
```

集成测试调用本平台 shell，验证管道、通配符、多行、Unicode、错误流、退出码、状态继承、截断后同步、可选超时、无限时命令取消，以及 HTTP 签发/复用/执行/撤销生命周期。所需 shell 缺失会让测试失败。CI 覆盖 Windows x86_64、Linux x86_64、macOS Apple Silicon，macOS 使用系统 Bash 3.2。

双 daemon 测试使用真实 loopback HTTP 验证路由、独立状态、令牌权限、到期、撤销和关闭级联。Linux/macOS CI 另用 `scripts/test-dual-daemon.py` 启动不同 UID 的真实进程，检查两种 `sudo` 模式的 `id -u` 输出；它不安装系统服务。

浏览器测试使用临时配置和虚拟 WebAuthn 认证器，验证注册、登录、签发、复制与撤销，不读取真实凭据。Win32 适配器测试覆盖真实原生控件、Unicode、DPI、到期和关闭回传。`cargo run --example desktop_smoke` 可打开一个不连接 daemon、不创建 shell、不会授予权限的桌面测试窗口；用于人工检查焦点和键盘操作。

`main` 分支 push 触发 `CI` 后，只有全部平台测试、构建并上传成品成功，`Release Candidate` 才会接续运行。它按平台分别下载该次 CI 成品（不重复构建），打包为 Windows ZIP、Linux/macOS tar.gz，生成 `SHA256SUMS`，并以 `v<项目版本>-rc.<CI序号>.<重试序号>` 创建 GitHub Release，显式关闭 Pre-release 标记并设为 Latest，方便从仓库首页下载。版本号和包内版本仍保留 RC 后缀，命令行更新使用 `--prerelease`。PR 检查不会发布 RC；正式版 `Release` 工作流也明确排除 `-rc.*` tag。重试发布用的 CI 时需重跑全部平台；打包阶段会拒绝不同重试序号的版本混装。

## License

MIT
