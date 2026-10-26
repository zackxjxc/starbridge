# StarBridge

StarBridge 是一个基于 FRP 的跨网络设备连接工具。客户端使用 Tauri 2 桌面窗口，中心服务器通过 HTTPS 提供账户、设备和连接控制服务，FRP 负责直连协商和中继回退。

一期先实现中心服务器远程组网：优先尝试 FRP XTCP 直连，失败后按本次连接策略回退到 FRPS 中继。控制台显示当前是“直连”还是“中转”，并允许用户在建联前选择是否允许直连。设备属性中可以设置“本机只允许直连”。

## 文档

- [一期详细设计](docs/phase-1-design.md)
- [架构总览](docs/architecture.md)
- [客户端 UI 与交互定稿（两栏石墨灰）](docs/client-ui-and-interaction-spec.md)
- [会话占用与准入规则](docs/client-experience-and-session-rules.md)
- [本机连接测试方案](docs/connection-self-test-plan.md)
- [FRP/OpenSSH 来源与打包](docs/dependencies-and-packaging.md)
- [二期、三期和未来扩展路线](docs/future-roadmap.md)
- [开发环境、服务器和部署约束](docs/development-environment.md)

## 开发阶段

客户端界面内嵌在固定 900×600 的 Tauri 窗口中，采用两栏石墨灰界面，通过 IPC 调用 Rust 核心；桌面模式不启动本地 HTTP 控制台，也不打开系统浏览器。正式客户端内置连接服务，普通用户只需注册或登录，默认进入本机页。开发联调可通过 `STARBRIDGE_SERVER_URL` 或无界面模式的配置 API 使用本机中心。依赖二进制在构建前按锁定版本下载、校验并复制进发布目录，不从运行中的用户电脑临时下载。

## 线上地址和端口

线上部署阶段使用以下地址：

```text
https://starbridge.zackxjxc.de5.net
https://starbridge.zackxjxc.de5.net/admin
```

公网 HTTPS 使用 443 端口，FRP 原生入口使用 17851 端口。HTTPS 和 FRP 地址由客户端与中心配置提供，普通界面不要求填写。17850 仅作为服务器本机开发或回环服务端口，不作为公网入口。

## 目录

```text
client/       端侧 StarBridge 核心和 Tauri 桌面入口
server/       中心 StarBridge 服务
client-ui/    内嵌桌面 UI，同时兼容无界面模式的 HTTP 控制台
admin-ui/     中心服务器管理 UI
vendor/       已锁定并校验的外部二进制和许可证
deploy/       本地和 Linux 部署配置
docs/         架构、打包和开发文档
```

旧版 wstunnel 验证项目保存在 `E:/Work/starbridge_old/StarBridge`，只作为参考，不参与新版本构建。

## 本机启动一期原型

当前仓库已经包含可运行的中心服务和 Tauri 客户端。桌面客户端需要 Rust 1.90+ 及对应平台的 [Tauri 构建依赖](https://v2.tauri.app/start/prerequisites/)；Windows 需要 C++ 构建工具和 WebView2。静态界面无需 Node.js 或前端开发服务器。先在一个终端启动中心服务：

```powershell
cargo run -p starbridge-server
```

再启动两台设备的客户端。本机联调使用两个独立数据目录：

```powershell
$env:STARBRIDGE_SERVER_URL = 'http://127.0.0.1:17850'
cargo run -p starbridge-client -- --data-dir state/node-a
cargo run -p starbridge-client -- --data-dir state/node-b
```

两个实例使用同一套客户端，都能在空闲时发起或接受连接。先注册账户，再在各设备上登录同一账户。设备列表显示同账户设备；点击设备只查看详情，进入配置并点击开始后才连接。A 连接 B 后可继续连接空闲的 C，但 B 不能反向连接或被其他设备重复接入。最小化保留任务栏入口；关闭窗口或按 Alt+F4 隐藏到托盘，心跳与连接继续运行。点击托盘图标恢复窗口，右键菜单仅有“退出”；有活动连接、等待任务或连接测试时退出需确认，随后停止心跳并回收受管的 `frpc`、`sshd` 子进程。托盘创建失败时关闭窗口恢复为正常退出。

本轮协议需要同步升级中心服务和客户端；历史设备身份和 SSH 密钥继续复用。构建后可运行 `python tools/test-peer-connectivity.py`，在隔离数据目录中验证 A→B/C 的真实 FRP/SSH 连接、占用拒绝、单台终止、同向复用和重启恢复。空闲后可以反过来由 B 连接 A，不允许两方向同时参与。

联调或自动化可添加 `--headless`，启动原有的本地 HTTP 控制台；`--no-browser` 保留为兼容入口。`powershell -ExecutionPolicy Bypass -File tools/start-local.ps1` 使用这种模式一次启动本地中心和两个设备实例。若只需要无界面客户端，可用 `cargo run -p starbridge-client --no-default-features -- --headless --data-dir state/node-a`，无需 Tauri 系统依赖。

目前还没有把真实 `frpc/frps` 二进制提交到仓库。先运行 `python tools/fetch_runtime.py --manifest vendor/manifest.toml --output state/runtime` 准备经过 SHA-256 校验的本机运行时，再用 `starbridge-server --frps-path state/runtime/frps.exe` 启动中心和本地 `frps`。`--frps-bind-addr`（默认 `127.0.0.1`）控制本机 frps 的监听地址，`--frps-addr`（也可写作 `--frps-advertised-addr`）是下发给客户端用于连接 frps 的可达地址；两者可按部署拓扑分别设置。端侧会自动寻找实例目录旁的共享 `state/runtime`，为 `direct_only`、`relay_only` 和 `direct_preferred` 生成受控的 XTCP/STCP 配置并监督 `frpc`；缺少运行时或启动失败时，控制台会明确显示原因。如果 Windows 安全软件拦截 `frpc.exe`，请根据界面提示手动检查并放行。

### Windows 便携版

在仓库根目录执行：

```powershell
python tools/fetch_runtime.py --manifest vendor/manifest.toml --output state/runtime
cargo build --release -p starbridge-client
python tools/package_windows.py
```

输出为 `dist/StarBridge-test/` 和 `dist/StarBridge-test.zip`。ZIP 根目录直接包含 `starbridge.exe`、`README.txt`、`runtime/` 和 `licenses/`，不再套一层包名目录。构建默认启用桌面和 `custom-protocol` 功能，将 UI 嵌入可执行文件；打包仅组装便携目录与 zip，不生成 MSI 或 NSIS 安装包。

完整解压到可写目录后，双击 `starbridge.exe`，使用用户名和密码登录或注册。连接服务已内置，不需要填写服务器地址。需要保留 `runtime/` 中的全部运行组件和 `runtime-manifest.json`，以及 `licenses/` 中的许可证文件。运行电脑需要已安装 WebView2 Runtime；不需要安装 StarBridge 或 Rust。

默认设备数据保存在当前用户的系统目录：Windows 为 `%LOCALAPPDATA%/StarBridge`，macOS 为 `~/Library/Application Support/StarBridge`。重新解压或更换程序目录会继续使用同一身份和实例锁。同账号通过系统机器标识派生的内部查找键复用设备，设备凭据单独验证；凭据冲突或设备已删除时，登录后必须输入账号密码并明确确认重新绑定，后台不会替换凭据或恢复设备。详见 [设备身份与迁移](docs/machine-identity.md)。

开发联调可继续使用不同的空 `--data-dir`，每个目录拥有独立测试身份和实例锁，避免覆盖默认正式设备；旧双实例启动脚本仍使用各自的显式目录。升级请保留旧数据目录，多个旧身份存在时用 `--import-data-dir PATH` 明确选择导入空默认目录，不要合并文件。`runtime/` 从数据目录及其父目录、可执行文件旁依次查找，兼容开发环境的 `state/runtime/`。发布 ZIP 仅打包程序、README、运行时和许可证，不含配置、身份、凭据或登录状态；BAT、PS1 启动及测试脚本留在仓库供开发使用，不随包分发。

### macOS（Apple Silicon）

macOS 使用 `vendor/manifest.darwin-arm64.toml`。FRP 下载官方预编译包；OpenSSH 没有官方 macOS 预编译包，系统自带的 `/usr/sbin/sshd` 复制出 `/usr` 后会被系统终止，所以构建时下载 portable 源码，编译出设备内置 sshd 需要的 `sshd`、`sshd-session`、`sshd-auth`、`ssh-keygen` 和 `sftp-server`。需要先安装 Xcode 命令行工具，用 Python 3.11+ 运行，或者在旧版本里安装 `tomli`：

```sh
python3 tools/fetch_runtime.py --manifest vendor/manifest.darwin-arm64.toml --output state/runtime
cargo run -p starbridge-server -- --frps-path state/runtime/frps
```

构建脚本会在未设置 `SDKROOT` 时自动选择当前 Xcode 的 SDK，OpenSSH 默认以 macOS 12 为最低系统版本。生成桌面 Release 和应用包：

```sh
cargo build --locked --release --workspace
python3 tools/package_macos.py --signing-identity "Developer ID Application"
```

输出为 `dist/StarBridge.app` 和 `dist/StarBridge.zip`，适用于 Apple Silicon、macOS 12+。解压后双击应用，或拖到“应用程序”目录；无需 Python、Rust 或独立安装 FRP/OpenSSH。包内包含经过锁定清单与文件哈希校验的客户端运行时及许可证，SSH 客户端使用系统自带的 `/usr/bin/ssh`。正式更新始终使用同一 Developer ID Application 签名证书；多个证书匹配时需指定准确名称或指纹。没有证书的开发环境可使用 `--signing-identity -` 生成本地 ad-hoc 包，但更新后可能需要重新授权钥匙串。签名后验证整个包，Apple 公证尚未执行。已有应用不会被打包脚本覆盖，可使用新的 `--output` 路径重新打包。

启动时静默读取已保存的凭据；如果系统拒绝访问设备身份，客户端显示恢复页面并保留原数据。只有点击「授权后重试」才弹出系统授权框；拒绝后留在页面上，不退出或自动重建身份。详见 [设备身份与迁移](docs/machine-identity.md)。

macOS 数据保存在 `~/Library/Application Support/StarBridge`，桌面和无界面模式共用默认身份与实例锁，升级或移动应用时保留；可通过 `--data-dir` 指定独立测试目录。验证本地三节点真实连接：

```sh
cargo test --locked --release --workspace
python3 tools/test-peer-connectivity.py --client target/release/starbridge-client --server target/release/starbridge-server
python3 -m unittest discover -s tools -p 'test_package_*.py'
```
