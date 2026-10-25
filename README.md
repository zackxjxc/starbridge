# StarBridge

StarBridge 是一个基于 FRP 的跨网络设备连接工具。客户端使用 Tauri 2 桌面窗口，中心服务器通过 HTTPS 提供账户、设备和连接控制服务，FRP 负责直连协商和中继回退。

一期先实现中心服务器远程组网：优先尝试 FRP XTCP 直连，失败后按本次连接策略回退到 FRPS 中继。控制台显示当前是“直连”还是“中转”，并允许用户在建联前选择是否允许直连。设备属性中可以设置“本机只允许直连”。

## 文档

- [一期详细设计](docs/phase-1-design.md)
- [架构总览](docs/architecture.md)
- [FRP/OpenSSH 来源与打包](docs/dependencies-and-packaging.md)
- [二期、三期和未来扩展路线](docs/future-roadmap.md)
- [开发环境、服务器和部署约束](docs/development-environment.md)

## 开发阶段

开发阶段中心服务器运行在本机，客户端使用 `localhost` 连接。客户端界面内嵌在 Tauri 窗口中，通过 IPC 调用 Rust 核心；桌面模式不启动本地 HTTP 控制台，也不打开系统浏览器。中心服务器和管理页面保持原有部署方式。依赖二进制在构建前按锁定版本下载、校验并复制进发布目录，不从运行中的用户电脑临时下载。

## 线上地址和端口

线上部署阶段使用以下地址：

```text
https://starbridge.zackxjxc.de5.net
https://starbridge.zackxjxc.de5.net/admin
```

公网 HTTPS 使用 443 端口，FRP 原生入口使用 17851 端口。用户只填写 HTTPS 地址，FRP 地址和端口由 StarBridge 在登录后自动配置。17850 仅作为服务器本机开发或回环服务端口，不作为公网入口。

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

再分别启动控制端和执行端。每个实例必须使用独立的数据目录：

```powershell
cargo run -p starbridge-client -- --role controller --data-dir state/controller-a
cargo run -p starbridge-client -- --role executor --data-dir state/executor-a
```

以上命令分别打开控制端、执行端窗口。登录时使用同一账户名称和密码；开发阶段中心服务会在首次登录时创建本地账户。端侧会完成登录、设备登记、心跳、设备列表和会话策略校验。关闭窗口会停止客户端的心跳任务和受管的 `frpc`、`sshd` 子进程。

联调或自动化可添加 `--headless`，启动原有的本地 HTTP 控制台；`--no-browser` 保留为兼容入口。`powershell -ExecutionPolicy Bypass -File tools/start-local.ps1` 使用这种模式一次启动本地中心、控制端和执行端。若只需要无界面客户端，可用 `cargo run -p starbridge-client --no-default-features -- --headless --data-dir state/controller-a`，无需 Tauri 系统依赖。

目前还没有把真实 `frpc/frps` 二进制提交到仓库。先运行 `python tools/fetch_runtime.py --manifest vendor/manifest.toml --output state/runtime` 准备经过 SHA-256 校验的本机运行时，再用 `starbridge-server --frps-path state/runtime/frps.exe` 启动中心和本地 `frps`。`--frps-bind-addr`（默认 `127.0.0.1`）控制本机 frps 的监听地址，`--frps-addr`（也可写作 `--frps-advertised-addr`）是下发给客户端用于连接 frps 的可达地址；两者可按部署拓扑分别设置。端侧会自动寻找实例目录旁的共享 `state/runtime`，为 `direct_only`、`relay_only` 和 `direct_preferred` 生成受控的 XTCP/STCP 配置并监督 `frpc`；缺少运行时或启动失败时，控制台会明确显示原因。如果 Windows 安全软件拦截 `frpc.exe`，请根据界面提示手动检查并放行。

### Windows 便携版

在仓库根目录执行：

```powershell
python tools/fetch_runtime.py --manifest vendor/manifest.toml --output state/runtime
cargo build --release -p starbridge-client
python tools/package_windows.py
```

输出为 `dist/StarBridge-test/` 和 `dist/StarBridge-test.zip`。构建默认启用桌面和 `custom-protocol` 功能，将 UI 嵌入可执行文件；打包仅组装便携目录与 zip，不生成 MSI 或 NSIS 安装包。

解压到可写目录后，双击 `starbridge.exe` 即可打开控制端；双击 `start-executor.bat` 打开执行端。首次在界面填写服务器地址，默认是本机开发地址。需要保留整个目录中的 `runtime/` 和许可证文件。运行电脑需要已安装 WebView2 Runtime；不需要安装 StarBridge 或 Rust。

默认数据分别保存到可执行文件旁的 `state/controller/` 和 `state/executor/`，不依赖启动时的工作目录；可通过 `--data-dir` 指定其他位置。`runtime/` 从数据目录及其父目录、可执行文件旁依次查找，兼容开发环境的 `state/runtime/`。升级时请保留数据目录，不要把配置、身份或登录状态打进分发 zip。

### macOS（Apple Silicon）

macOS 使用 `vendor/manifest.darwin-arm64.toml`。FRP 下载官方预编译包；OpenSSH 没有官方 macOS 预编译包，系统自带的 `/usr/sbin/sshd` 复制出 `/usr` 后会被系统终止，所以构建时下载 portable 源码，编译出执行端内置 sshd 需要的 `sshd`、`sshd-session`、`sshd-auth`、`ssh-keygen` 和 `sftp-server`。需要先安装 Xcode 命令行工具，用 Python 3.11+ 运行，或者在旧版本里安装 `tomli`：

```sh
python3 tools/fetch_runtime.py --manifest vendor/manifest.darwin-arm64.toml --output state/runtime
cargo run -p starbridge-server -- --frps-path state/runtime/frps
```

如果编译时报 `C compiler cannot create executables`，通常是命令行工具里的 SDK 和编译器版本不匹配，改用 Xcode 自带的 SDK：`SDKROOT=$(xcrun --sdk macosx --show-sdk-path) python3 tools/fetch_runtime.py ...`。
