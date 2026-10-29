# trustd

> 把信任交给 AI，不必把原始凭据交进对话。

用户希望 AI 可以提权、可以使用自己的凭据、可以代表自己执行操作。这些需求的本质，都是信任。

trustd 读作 trusted，末尾的 d 仍然代表 daemon。它给 AI 提供接受这份信任的途径，也让用户可以相当轻松地放权：说明要做什么、选择需要的身份和凭据、由用户授权，剩下的交给程序处理。

在极端安全、极端受限与直接暴露原始凭据之间，trustd 尝试找一个平衡点。它不要求每条命令都重新审批，也不要求模型复制令牌和会话密钥。用户决定授权范围与何时收回；程序保管授权状态、注入凭据，并以明确的操作系统身份执行。

唯一的行为约定：**尊重用户的个人隐私。**

## 日常使用

1. AI 说明任务和需要的凭据名称。
2. 用户通过桌面窗口或 Telegram 接受、拒绝，或记住选择。
3. AI 执行命令，程序自动附带授权并注入所选凭据。
4. 完成后关闭会话；用户也可随时在管理页面撤销授权。

例如，授权使用 `JENKINS_CI_TOKEN` 后，PowerShell 命令引用 `$env:JENKINS_CI_TOKEN`，Bash 命令引用 `$JENKINS_CI_TOKEN`。正常调用链不需要把值写入工具参数或对话。

这仍然是用户授权的命令执行能力，不是一个“能够使用秘密、却绝不可能读出秘密”的沙箱。命令可以主动打印环境变量；trustd 目前不自动脱敏任意命令输出。用户亲自交进来的凭据，也应当保留查看明文和导出的选项。

## 程序各司其职

| 程序 | 职责 |
| --- | --- |
| `trustd` | 安装、初始化、修复、更新、卸载与维护向导；不运行 daemon |
| `trustd-system` | Windows 专用，使用真正的 LocalSystem 身份运行 |
| `trustd-root` | macOS / Linux 专用，使用 root 身份运行 |
| `trustd-user` | 当前普通用户 daemon、桌面授权与用户凭据库 |
| `trustd-stdio` | Agent 宿主启动的 MCP stdio 接入程序，内部保管 handle |

Windows 包含 `trustd-system.exe`，macOS/Linux 包含 `trustd-root`；每个平台的发布包共有四个程序。不要只复制其中一个。独立云同步后端为 `localshelld-sync-server`，沿用原有部署身份与协议。

系统 daemon 默认监听 `127.0.0.1:32119`，用户 daemon 默认监听 `127.0.0.1:32120`。维护向导使用临时本机端口，stdio 不监听端口。安装、更新不通过 daemon 的 HTTP/MCP 执行接口提供。

## 安装

下载并解压完整平台发布包，从对应入口开始：

| 平台 | 安装入口 |
| --- | --- |
| Windows | 双击 `INSTALL.cmd` |
| macOS | 双击 `INSTALL.command` |
| Linux | 在解压目录运行 `./INSTALL.sh` |

包内 `START-HERE.txt` 提供简短说明。四个程序是配套组件，无需逐个运行，也不要单独拖出安装入口。仍可用 `trustd` 或 `trustd maintenance` 启动向导。以普通登录用户运行，向导会在需要时请求 UAC、macOS 管理员授权或 Linux polkit 授权。

前置条件：

- Windows 10/11：系统级 [PowerShell 7](https://learn.microsoft.com/powershell/scripting/install/installing-powershell)，即 Program Files 下的 `PowerShell/7/pwsh.exe`。桌面授权使用原生 Win32 控件，不依赖 WebView2。
- Linux：`/bin/bash`、systemd；图形授权使用 GTK 3 / WebKitGTK 4.1，向导提权需要 `pkexec`。
- macOS：系统 `/bin/bash` 与 launchd，兼容 Bash 3.2；预编译包提供 Apple Silicon。

向导负责初始化、安装或修复、旧安装迁移及服务卸载。首次安装后，在两个管理页面完成显式配对：

1. 登录 [用户管理页](http://localhost:32120/)，在桌面授权中生成配对码。
2. 登录 [系统管理页](http://localhost:32119/)，填入用户 daemon 地址与配对码。
3. 将 Agent 连接到用户入口，或启动下面的 stdio 接入程序。

配对码只用于两个 daemon 建立互信，不是交给 AI 的执行令牌。已有密码、TOTP、通行密钥、配对、凭据和授权记忆不会因改名或修复而重新初始化。

| 平台 | 程序目录 | 系统配置 |
| --- | --- | --- |
| Windows | `Program Files/trustd` | `ProgramData/localshelld/system/config.toml` |
| macOS | `/Library/trustd` | `/Library/Application Support/localshelld/system/config.toml` |
| Linux | `/opt/trustd` | `/etc/localshelld/system/config.toml` |

每套程序安装到 `versions/<整套摘要>/`。摘要覆盖四个程序的文件名与内容；提权前暂存、提权后安装均重新校验。程序目录可读、可执行，但普通用户不可写；系统配置和备份仅供系统管理员访问。用户数据仍在原有 `localshelld-user` 平台目录，安装不修改 PATH。

macOS 的完整程序路径是 `/Library/trustd/versions/<整套摘要>/`，用户配置位于 `~/Library/Application Support/dev.localshelld.localshelld-user/config.toml`，维护入口是 `~/Applications/trustd.app`。系统服务由 `/Library/LaunchDaemons/dev.localshelld.plist` 注册。数据目录沿用 `localshelld` 名称以兼容已有安装。

迁移保留旧程序，备份原配置、`config.authorization.json` 和同目录的 `seal.key`。系统安装失败会尝试恢复原注册；用户自启动失败会明确报告，可以重新修复。卸载只停止并注销服务，保留程序、配置和备份。更新和重启会中断执行会话，使相应 daemon 签发的运行期令牌失效。

`trustd maintenance --no-open` 只打印向导地址。本次链接是临时管理凭据，不应分享；退出向导或 30 分钟后结束监听。

## 接入 AI：推荐 stdio

向导的“安装选项”可自动配置 Codex、Claude Code、Claude Desktop、Cursor、Windsurf、VS Code、Gemini CLI 和 Devin Desktop。默认不勾选；只写入所选客户端的用户级配置。VS Code 使用默认本地用户配置，便携版、自定义 profile 和远端配置需手动接入。

配置指向本次安装目录内的 `trustd-stdio`，使用实际用户 daemon 地址。其他条目和注释保留，已有文件先保存同目录的 `*.trustd-backup-*` 备份；替换已有 `trustd` 条目需另行勾选。各客户端的结果和备份位置分别显示，单个配置失败不影响服务安装。升级后可重新勾选客户端以切换到新版本，旧版本目录不会自动删除。完成后重启客户端。

配置格式参照 [Codex MCP](https://learn.chatgpt.com/docs/extend/mcp)、[Claude Code MCP](https://code.claude.com/docs/en/mcp)、[VS Code MCP](https://code.visualstudio.com/docs/agents/reference/mcp-configuration) 等官方文档。向导只写配置，不修改客户端的工具审批设置。

在支持 stdio 的 MCP 宿主中配置完整安装路径：

```json
{
  "mcpServers": {
    "trustd": {
      "command": "C:\\path\\to\\trustd-stdio.exe",
      "args": ["--endpoint", "127.0.0.1:32120"]
    }
  }
}
```

macOS/Linux 将 `command` 改为 `trustd-stdio` 的绝对路径。默认 endpoint 就是 `127.0.0.1:32120`，可以省略参数。它只接受字面量 loopback 地址，不使用 HTTP 代理、不跟随重定向。

默认工具是 `secrets_list`、`secrets_get`、`enter_interactive`、`run`、`destroy`。交互授权示例：

```json
{
  "agent_name": "Example Agent",
  "request_reason": "查询用户指定的 Jenkins 构建。",
  "confirm_text": "OK",
  "secret_keys": ["JENKINS_CI_TOKEN"]
}
```

授权结果保留执行身份、有效期、`status` 和用户附言 `btw`，不返回 handle。随后执行：

```json
{"command":"whoami","privilege_level":"current-user"}
```

结束时调用 `destroy({})`。这两个调用都没有 handle 参数。

### 隔离与生命周期

- 一个 stdio 接入进程持有一个会话。两个独立进程不会使用全局“最近一次授权”。同一宿主若让多个聊天共用一个接入进程，它们也会共享这个会话；聊天级隔离需要宿主分别启动进程。
- 已有会话时再次进入会被拒绝。更换凭据选择前先 `destroy`，授权失效后不会静默换成别的身份或授权。
- `notifications/cancelled` 按本进程内的请求 ID 取消。取消执行会销毁整个会话及其进程树，不需要传 MCP 会话头或 handle。
- stdin EOF 或正常终止会尝试关闭会话。尚在等待批准的请求会接收迟到的结果并清理；强杀进程、断电、daemon 不可达时无法保证清理成功，必要时在管理页撤销令牌。
- stdio 的 stdout 仅承载换行分隔的 JSON-RPC，诊断写入 stderr。对结果未知的执行请求，不自动重试。

### 预授权也不必经过模型

用户可以把执行 token 配置到启动环境，再传 `--token-env TRUSTD_TOKEN`。这里只传环境变量名，值不在命令行中。

配置后多出 `enter_preauthorized` 与 `revoke`，两者都没有 token 参数。前者仍需要 `confirm_text: "OK"`，可选 `secret_keys`；后者撤销配置的 token。每个接入进程使用单独的 token，不要跨进程或与 HTTP 调用者共享；检测到 token 已有存活会话时拒绝接管，也不销毁别人的会话。

## 执行身份与会话

| 平台 | 参数 | 实际身份 |
| --- | --- | --- |
| Windows | `privilege_level: "current-user"` | 普通登录用户及其环境 |
| Windows | `privilege_level: "as-administrator"` | 同一用户的管理员令牌、USERPROFILE、HKCU 与用户 PATH |
| Windows | `privilege_level: "nt-authority/system"` | LocalSystem，使用系统账户环境 |
| macOS/Linux | `sudo: false` / `true` | 当前用户 / root |

执行身份必填，不会自动提权或换身份重试。Windows 管理员身份需要已配对用户 daemon 和该用户可用的 linked administrator token；SYSTEM 不会冒充管理员用户。用户 daemon 拒绝以 root/管理员运行；`trustd-system` 拒绝普通管理员身份。

每种身份分别保留变量、函数、目录与环境。Windows 使用 `pwsh -NoProfile -NonInteractive`，Unix 使用 `/bin/bash --noprofile --norc`。命令交给原生 shell 解析，支持多行、管道、Unicode 和通配符。

命令默认不限时，可显式提供正整数 `timeout_seconds`；超时或取消会关闭整个会话并返回部分输出。普通命令的非零退出码不会关闭会话。可选 `stdin` 是一次性 UTF-8 输入，随后 EOF；没有持续输入、PTY、后台任务或轮询 API。任务调度仍由 Agent 宿主负责。

响应包含独立的 `stdout` / `stderr`、实际 `identity`、退出码、耗时、目录和会话结束原因。仅凭 stderr 非空不能判断失败。Windows 通过 Job Object 回收进程树；Unix 回收可追踪后代与原进程组，不保证回收已彻底脱离并被重新托管的 daemon。

## 凭据与授权管理

管理页面支持 Master Password、TOTP 与通行密钥。密码只保存 Argon2id verifier；TOTP 和环境变量凭据使用 AES-256-GCM 加密。通行密钥请在 `http://localhost:32119/` 或 `http://localhost:32120/` 注册，两个入口分别管理。

凭据库使用受文件权限保护的 `seal.key`，不依赖系统钥匙串，也不会触发系统保险库的授权弹窗。配置文件、授权状态文件和 seal key 应一起备份；密文与密钥一同失窃时，文件加密不能提供独立保护。

凭据列表只返回名称。用户选择的 key 在进入会话时形成快照，直接注入相应 shell 环境；后台轮换凭据不会改变已打开的会话，需关闭并重新授权。

明文选项保留：

- 管理页可导出、导入明文 JSON；导入会替换全部凭据，需要先确认备份。
- 用户明确要求时，`secrets_get` 可读取当前存活会话已授权 key 的值。stdio 只需 `{"key":"JENKINS_CI_TOKEN"}`；HTTP/MCP 直连还需 handle。
- 平常优先使用环境变量注入，而不是先把凭据读进对话。

默认使用离线库，也可连接自托管同步后端。连接后以云端为准，版本冲突拒绝覆盖，断网不悄悄改用缓存完成凭据操作。同步不是端到端加密；协议、恢复和部署方式见 [同步文档](docs/SYNC.md)。桌面与 Telegram 授权可并行使用，配置见 [Telegram 授权](docs/telegram.md)。

交互授权限时 60 秒；忽略或关闭窗口仍等待原截止时间。名称是代理自报信息，不是身份认证；记忆选择按名称和已授权 key 范围生效。新增 key 会重新请求同意。记忆接受不声称本次发生了点击，记忆拒绝返回 `automatic: true`。用户可清除记忆，也可单独撤销已建立的授权。

## HTTP 与直接 MCP

已有 HTTP 与 Streamable HTTP MCP 客户端继续工作：

- `http://127.0.0.1:32120/mcp`：用户入口。
- `http://127.0.0.1:32119/mcp`：系统入口。
- 两端的 `/llms.txt` 和 `/v1/capabilities` 无需鉴权，返回说明及实际接口 schema。

直接连接 daemon 时，token/handle 仍然是必要的执行凭据；只有 stdio 适配层替调用者保管它们。HTTP 断连不自动取消执行；HTTP MCP 取消仍须使用初始化响应的 `Mcp-Session-Id`。完整接口、错误和取消语义见 [API 文档](docs/API.md)。

## 更新与旧安装迁移

```text
trustd update --check
trustd update --prerelease
trustd update --tag v0.1.0-rc.8.1
trustd update --download-only
```

`--check` 只查询，不下载或停服。正常更新从普通用户终端发起：下载完整套件，验证 SHA-256、平台与每个程序的版本，然后启动新版本维护向导。**向导完成才算更新成功**，启动向导不代表服务已经切换。`--download-only` 只准备已校验套件，适合无图形环境；命令会输出其目录。实际更新应在独立终端运行，不在 daemon 创建的执行会话内执行。

稳定通道排除所有 RC；要包含 RC 使用 `--prerelease`。降级必须显式指定 `--tag` 与 `--allow-downgrade`。发布源为 `hatsune-miku/trustd` GitHub Releases；下载使用系统/环境 HTTP 代理、HTTPS 和发布包校验值，尚无独立发布签名。

从 localshelld 迁移时，解压完整 trustd 套件并运行维护向导，不要用旧的单文件更新器替换某个 exe。下列标识为兼容已有数据与部署而保留：

- 系统服务 `localshelld` / `localshelld.service` / `dev.localshelld`，以及原用户自启动项；
- 配置目录、seal key、凭据导出格式 `localshelld-credentials`、同步和配对协议；
- `LOCALSHELLD_*` 构建/运行期环境变量、内部 Rust 库名与兼容工具别名；
- 健康接口的 `service: "localshelld"`，另以 `product: "trustd"` 标示产品名。

不要覆盖 macOS 自带的 trustd。本项目 root daemon 名为 `trustd-root`，使用自己的安装目录和原有 `dev.localshelld` launchd 标签。

## 源码构建与高级 CLI

需要 Rust stable、Node.js 22、pnpm 10.14。Linux 构建依赖 `libwebkit2gtk-4.1-dev libgtk-3-dev libssl-dev pkg-config`；Windows 的 OpenSSL 构建需要 Perl/NASM。

```powershell
pnpm --dir web install --frozen-lockfile
pnpm --dir web build
cargo build --release
./target/release/trustd maintenance
```

开发时可只启动用户端，无需提权：

```powershell
./target/release/trustd init --user --config ./user.local.toml
./target/release/trustd-user --headless --config ./user.local.toml
./target/release/trustd-stdio
```

低层 `trustd install` 注册同目录的专用 daemon，不负责移动程序和收紧自定义目录。系统配置需先 `trustd init --config <绝对路径>`；完整安装需从目标登录用户的管理员终端或 sudo 会话执行，初始化用户端后立即启动两端，但不自动配对。`--system-only` 仅安装系统服务，`--user` 仅安装用户自启动，`--user-config` 指定完整安装的用户配置。旧注册路径不匹配时保留原配置并报错，迁移使用向导。

`trustd uninstall` 默认移除两端，`--system-only` 或 `--user` 限定范围。Windows 用户任务可能需要同一账户的管理员权限；SYSTEM/Session 0 不能替代登录用户管理用户任务。卸载不按端口查杀手工启动的进程。

初始化支持空密码并保留空格，`--password-stdin` 读取一行；`--totp` 先显示 URI/secret，再要求动态码确认。运行 daemon 的程序仅接受自身配置/运行参数，不提供 init、install 或 update。

## 验证

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
pnpm --dir web format:check
pnpm --dir web test
pnpm --dir web test:e2e
```

测试覆盖 HTTP/MCP 授权、原生 shell、凭据快照、双 daemon 生命周期、stdio 会话隔离/取消/EOF 清理，以及整套程序校验。CI 在 Windows x86_64、Linux x86_64、macOS Apple Silicon 构建并检查平台所需的全部程序；只有各平台成功后才发布完整 RC 套件与 SHA256SUMS。

设计取舍见 [架构文档](docs/ARCHITECTURE.md)。

## License

MIT
