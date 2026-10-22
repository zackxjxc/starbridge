# FRP 与 OpenSSH 的来源和打包方案

## 结论

FRP 和 OpenSSH 都作为外部运行时组件随 StarBridge 发布，但不直接把“下载最新版”写进用户首次启动流程。发布构建使用明确的版本清单，先下载对应平台的压缩包或安装包，验证 SHA-256 和签名（项目提供时），再将需要的二进制、许可证和来源信息复制到发布目录。这样用户安装包可以离线安装，构建结果可复现，也不会因为上游发布新版本而悄悄改变行为。

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

Windows 客户端优先使用 Win32-OpenSSH 的稳定发布包，至少需要 `ssh.exe`；需要文件传输时同时带 `sftp.exe` 和 `scp.exe`。`sshd.exe` 和 `sftp-server.exe` 是执行端作为 SSH 服务端时才需要的组件。Linux 端优先使用发行版自带的 OpenSSH，不把一个 Windows 版 OpenSSH 打进 Linux 服务器包。

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

## 发布目录

Windows 客户端建议布局：

```text
StarBridge/
  starbridge.exe
  runtime/
    frpc.exe
    ssh.exe
    sftp.exe
    scp.exe
  licenses/
    frp/
    openssh/
  runtime-manifest.json
```

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

## 许可证和升级责任

发布物必须包含 FRP、OpenSSH 及其打包中实际携带的第三方许可证和 NOTICE 文件。每次依赖升级都要记录上游版本、下载 URL、SHA-256、发布日期和测试结果。安全漏洞修复优先升级依赖并重新发布，不能在运行时静默替换二进制。
