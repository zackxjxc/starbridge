# FRP 与 OpenSSH 的来源和打包方案

## 结论

FRP 和 OpenSSH 都作为外部运行时组件随 StarBridge 发布，但不直接把“下载最新版”写进用户首次启动流程。发布构建使用明确的版本清单，先下载对应平台的压缩包或安装包，验证 SHA-256 和签名（项目提供时），再将需要的二进制、许可证和来源信息复制到发布目录。Windows 客户端使用 Tauri 2，发布为解压后双击运行的便携目录和 zip，不制作 MSI 或 NSIS 安装包；中心服务器的发布方式保持不变。

## 官方来源

FRP：

- 项目仓库：<https://github.com/fatedier/frp>
- 官方 Release 页面：<https://github.com/fatedier/frp/releases>
- 官方文档：<https://gofrp.org/en/docs/>
- XTCP 文档：<https://gofrp.org/en/docs/features/xtcp/>

FRP 发布包通常同时包含 `frpc` 和 `frps`。端侧发布物只带 `frpc`，中心 Linux 部署包带 `frps`；不要把 `frps` 放进普通用户客户端安装目录。

OpenSSH：

- portable 源码仓库：<https://github.com/openssh/openssh-portable>
- portable 源码发布镜像：<https://www.openssh.org/ftp.html>
- Windows 发行版和发布页：<https://github.com/PowerShell/Win32-OpenSSH/releases>
- Windows 安装说明：<https://github.com/PowerShell/Win32-OpenSSH/wiki/Install-Win32-OpenSSH>

Windows 客户端优先使用 Win32-OpenSSH 的稳定发布包，至少需要 `ssh.exe`；需要文件传输时同时带 `sftp.exe` 和 `scp.exe`。`sshd.exe` 和 `sftp-server.exe` 供每台设备提供 SSH 服务的组件。Linux 端优先使用发行版自带的 OpenSSH，不把一个 Windows 版 OpenSSH 打进 Linux 服务器包。

OpenSSH 的许可证、第三方库和各平台发布说明必须随发布物保存。不要从不明的第三方“绿色版 SSH”下载可执行文件。

## Windows 杀毒软件提示

Windows 上运行 FRP 时，部分杀毒软件或终端安全软件可能把 `frpc.exe`、`frps.exe` 或其网络行为误判为风险程序。当前一期仍然选择 FRP；这类误报暂不作为更换 FRP 或改变总体架构的理由。

发布说明和客户端 UI 应明确提示用户：如果 FRP 启动失败或连接异常，请先检查杀毒软件、Windows Defender 或企业终端安全策略，按用户自己的安全判断手动放行对应文件，或将 StarBridge 安装目录加入白名单。StarBridge 不应静默关闭安全软件，也不应自动修改系统安全策略。

## 版本清单

在仓库中维护机器可读的依赖锁定文件，例如 `vendor/manifest.toml`：

```toml
[[artifact]]
name = "frp"
version = "vX.Y.Z"
platform = "windows-x86_64"
url = "https://github.com/fatedier/frp/releases/download/vX.Y.Z/frp_X.Y.Z_windows_amd64.zip"
sha256 = "..."

[[artifact]]
name = "openssh"
version = "vA.B.C"
platform = "windows-x86_64"
url = "https://github.com/PowerShell/Win32-OpenSSH/releases/download/vA.B.C/OpenSSH-Win64.zip"
sha256 = "..."
```

上面的版本号、文件名和 SHA-256 只是格式示例，不能直接作为真实依赖使用；首次接入时由发布脚本读取官方 Release 页面，填写已验证的具体版本和完整 URL，并将下载结果锁定。版本升级必须修改清单、重新校验、运行连接回归测试并提交变更。

## 构建流水线

建议提供一个 `tools/fetch-runtime`（Rust、PowerShell 或 Python 均可）脚本，步骤固定为：

1. 读取 `vendor/manifest.toml`。
2. 将文件下载到构建缓存目录，不直接覆盖工作区中的发布目录。
3. 校验 HTTPS 下载结果的 SHA-256；校验失败立即终止。
4. 解压到临时目录，只复制允许的文件名和架构匹配的文件。
5. 复制许可证、来源 URL、版本和校验值到 `vendor/licenses/` 与发布元数据。
6. 对 Windows PE 和 Linux ELF 做架构检查，并执行 `frpc --version`、`frps --version`、`ssh -V` 等基本检查。
7. 由打包步骤把结果放入最终发布目录；构建缓存可复用，但运行时不依赖缓存。

CI 应缓存下载文件但仍每次校验哈希。生产发布使用已审核的锁定清单；开发者可以显式更新清单，但不能由“latest”别名自动决定版本。

Windows 客户端构建需要 Rust 1.90+、C++ 构建工具和 WebView2，详见 [Tauri 前置依赖](https://v2.tauri.app/start/prerequisites/)。客户端直接嵌入 `client-ui/` 静态资源，无需 Node.js 或前端开发服务器。在仓库根目录运行：

```powershell
python tools/fetch_runtime.py --manifest vendor/manifest.toml --output state/runtime
cargo build --release -p starbridge-client
python tools/package_windows.py
```

默认功能包含 `desktop` 和 `custom-protocol`，普通 Cargo release 构建即可生成内嵌界面的 exe。Tauri 配置关闭安装包生成；`tools/package_windows.py` 将可执行文件、客户端运行时和许可证组装为 `dist/StarBridge-test/` 与 `dist/StarBridge-test.zip`。发布物不包含 `frps` 或服务器二进制。若使用 Tauri CLI，应按 [Tauri 分发文档](https://v2.tauri.app/distribute/) 跳过安装包步骤。

## 发布目录

Windows 客户端便携目录布局：

```text
StarBridge-test/
  starbridge.exe
  start-node.bat
  start-controller.bat       兼容旧数据目录
  start-executor.bat         兼容旧数据目录
  runtime/
    frpc.exe
    ssh.exe
    sftp.exe
    scp.exe
    ssh-keygen.exe
    sshd.exe
    sshd-session.exe
    sshd-auth.exe
    sftp-server.exe
    ssh-shellhost.exe
    libcrypto.dll
  licenses/
    frp/
    openssh/
  runtime-manifest.json
  state/                   首次运行时创建，不进入分发 zip
    node/
```

双击 `starbridge.exe` 或 `start-node.bat` 打开设备中心，同账户设备可互相连接。旧启动脚本仍可使用，并继续访问原来的数据目录。首次在界面配置中心服务器地址；默认值是本机开发地址。将整个便携目录放在可写位置，保留 `runtime/` 和许可证，运行电脑需已有 WebView2 Runtime；StarBridge 不运行安装器。默认数据目录基于 exe 位置，`--data-dir` 可覆盖；同时运行的实例不得共用数据目录。升级请保留已有 `state/`，不要将身份、密钥或登录状态混入发布物。

中心 Linux 部署包建议布局：

```text
/opt/starbridge/bin/starbridge-server
/opt/starbridge/bin/frps
/opt/starbridge/licenses/
/opt/starbridge/runtime-manifest.json
```

运行时只允许 StarBridge 启动发布目录中已经校验过的子组件，并使用绝对路径。用户可以选择使用系统已有的 `ssh`/`sftp`，但 UI 必须显示实际路径和版本；FRP 版本由 StarBridge 发布包固定，不从系统 PATH 随意拾取。

## 平台策略

一期优先支持 Windows x86_64 客户端和 Linux ARM64 中心服务器，同时保留 Linux x86_64、Windows ARM64 和 macOS 的目录与清单扩展点。每个平台必须单独锁定 FRP 和 OpenSSH 包，不能把 Windows 可执行文件复制到 Linux 包中。

macOS ARM64 的清单是 `vendor/manifest.darwin-arm64.toml`：

- FRP 使用官方 `frp_<版本>_darwin_arm64.tar.gz`，SHA-256 取自同一 Release 的 `frp_sha256_checksums.txt`。
- OpenSSH 没有官方 macOS 预编译包；系统自带的 `/usr/sbin/sshd` 带 Apple 平台签名，复制到其他目录后启动会被系统终止（退出码 137）。因此清单下载 `openssh-portable` 源码，`build = "openssh-portable"` 让 `tools/fetch_runtime.py` 在构建时用 `--without-openssl` 编译。StarBridge 只用 ed25519 密钥，OpenSSH 内置的加密实现足够，不依赖 macOS 不提供头文件的 OpenSSL。
- 源码构建的 `sshd` 会把 `sshd-session` 和 `sshd-auth` 的路径编译成安装前缀，端侧生成 `sshd_config` 时用 `SshdSessionPath`、`SshdAuthPath` 指回运行时目录。
- 构建脚本在未设置 `SDKROOT` 时通过 `xcrun` 选择当前 Xcode 的 SDK，并默认设置 `MACOSX_DEPLOYMENT_TARGET=12.0`，避免 OpenSSH 只能在构建机的系统版本上运行。FRP 的最低系统版本为 macOS 12。
- OpenSSH 源码包的 SHA-256 是从 `cdn.openbsd.org` 和 `mirrors.aliyun.com` 分别下载、结果一致后锁定的。上游还提供 `.asc` 签名，当前开发机没有 `gpg`，签名尚未校验。

`cargo build --locked --release --workspace` 编译客户端与本地测试中心后，执行 `python3 tools/package_macos.py`，得到 `dist/StarBridge.app` 和 `dist/StarBridge.zip`。打包前核对来源归档与锁定清单、运行时文件 SHA-256 和 ARM64 架构；包中仅保留 `frpc`、五个 OpenSSH 服务组件和许可证，不包含 `frps`、中心程序或用户状态。SSH 客户端使用 macOS 系统提供的 `ssh`。

应用主程序位于 `Contents/MacOS/starbridge`，运行时位于 `Contents/Resources/runtime/`。桌面数据位于用户的 `~/Library/Application Support/net.zackxjxc.starbridge/state/`，不会写入应用包。打包脚本对运行时和应用进行 ad-hoc 签名，签名后重新记录分发文件哈希并验证整个包；这是本地构建包，尚未进行 Developer ID 签名或 Apple 公证。ZIP 保留执行权限，脚本拒绝覆盖已有 `.app`。

## 待处理项

### 调试联通文件下载路径

在 macOS 开发机上，GitHub Release 下载 FRP 很不稳定：多次出现 `Failed to connect to github.com port 443`、`Connection reset by peer` 和 HTTP/2 帧错误，12.9 MB 的包靠断点续传约 15 分钟才下完。OpenSSH 从 `cdn.openbsd.org` 和 `mirrors.aliyun.com` 下载都正常。

需要确认的事项：

- 构建机访问 `github.com/fatedier/frp/releases` 的稳定方式，例如允许在清单里配置备用镜像地址，SHA-256 保持不变。
- `tools/fetch_runtime.py` 目前用 `urllib.request.urlretrieve` 一次性下载，没有超时、重试和断点续传，网络差时会长时间卡住或失败。
- 用 `gpg` 校验 OpenSSH 源码包的 `.asc` 签名，并把校验步骤加进构建流程。

## 许可证和升级责任

发布物必须包含 FRP、OpenSSH 及其打包中实际携带的第三方许可证和 NOTICE 文件。每次依赖升级都要记录上游版本、下载 URL、SHA-256、发布日期和测试结果。安全漏洞修复优先升级依赖并重新发布，不能在运行时静默替换二进制。
