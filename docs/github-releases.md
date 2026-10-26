# GitHub 自动打包

`.github/workflows/release.yml` 在推送 tag 时自动构建，五个包全部成功后发布到该 tag 的 GitHub Release，并附上 `SHA256SUMS.txt`。普通分支推送不触发；也可从 Actions 的 **Build release packages → Run workflow** 手动构建，手动运行只生成可下载的 Actions artifacts，不创建 Release；最终 `release-preview` artifact 包含五个包与校验文件。

## 五个发布包

| 用途 | 操作系统与架构 | 文件名（以 0.1.0 为例） |
| --- | --- | --- |
| 桌面客户端 | Windows x64 | `StarBridge-client-0.1.0-windows-x86_64.zip` |
| 桌面客户端 | macOS Apple Silicon ARM64 | `StarBridge-client-0.1.0-darwin-arm64.zip` |
| 中心服务端 | Windows x64 | `StarBridge-server-0.1.0-windows-x86_64.zip` |
| 中心服务端 | macOS Apple Silicon ARM64 | `StarBridge-server-0.1.0-darwin-arm64.tar.gz` |
| 中心服务端 | Linux ARM64 | `StarBridge-server-0.1.0-linux-arm64.tar.gz` |

架构与仓库当前锁定运行时一致；Linux ARM64 也与现有生产部署一致。这五个包不包含 Intel Mac、Windows ARM64 或 Linux x64 版本。Linux 包在 Ubuntu 24.04 ARM64 上原生构建，需要 glibc 2.39 或更高版本，不是 musl 静态包。

Windows 客户端是便携 ZIP，包含图形客户端、SSH 会话启动器、FRPC、OpenSSH 和许可证；运行需要 WebView2 Runtime。macOS 客户端 ZIP 包含 `.app`、ARM64 FRPC、从锁定源码构建的 OpenSSH 和许可证，需要 macOS 12 或更高版本。

macOS 客户端使用现有脚本的 ad-hoc 签名，未配置 Developer ID 证书和 Apple 公证；首次打开可能被 Gatekeeper 拦截，跨版本钥匙串授权行为仍需按 [macOS 发布检查清单](macos-release-checklist.md) 做实机验收。Windows 程序也未配置代码签名。

服务端包包含 `starbridge-server`、同平台的 `runtime/frps`、来源与 SHA-256 清单、FRP 许可证和中英文启动说明；不包含 OpenSSH、账号状态、生产配置、证书或私钥。解压后按包内 `README.txt` 设置公共域名、随机 FRP 令牌和 HTTPS 代理，再启动中心服务。

## 发布步骤

1. 修改根目录 `Cargo.toml` 的 `[workspace.package].version`，并用 `cargo check --workspace` 更新 `Cargo.lock`。提交这两个文件和本次发布所需代码。首次发布现有 `0.1.0` 时无需修改版本。
2. 在已经包含工作流的提交上创建版本 tag。tag 必须为 `v版本号` 或 `版本号`，并与 Cargo 版本完全一致，例如：

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

3. 在 GitHub Actions 查看构建。Windows、macOS 和 Linux 分别在原生 runner 上执行；Windows 和 macOS 各生成客户端、服务端两个包，Linux 生成一个服务端包。
4. 全部成功后，在仓库 Releases 下载五个包和校验文件。Linux/macOS 可用 `sha256sum -c SHA256SUMS.txt`（macOS 使用 `shasum -a 256 -c SHA256SUMS.txt`）检查下载完整性。

tag 与版本不符、测试失败、依赖 SHA-256 不符、服务端或 FRPS 无法在构建平台运行、发布物缺失或多出文件时，流程失败且不执行发布步骤。单个平台失败不会取消其他平台构建，可先查看各自结果，再从失败步骤重跑。成功包在 Actions artifacts 保留 14 天。发布阶段会重新核对五个包的集合并生成校验和；新 Release 先作为草稿上传附件，上传成功后才公开，同一 tag 重跑时更新已有 Release 的同名附件。

构建固定使用 Rust 1.94.1、Python 3.12 和版本锁定的官方运行时；官方 Actions 固定到提交 SHA。无需额外 PAT 或签名 secrets，只有最终发布 job 获得 `contents: write` 权限；仓库或组织策略需允许 GitHub Actions 运行以及写入 Release。

## 本地验证

```sh
python -m unittest discover -s tools -p 'test_package*.py' -v
python tools/release_metadata.py version
```

服务端打包脚本可单独使用，例如 Windows：

```sh
cargo build --release --locked -p starbridge-server
python tools/package_server.py --binary target/release/starbridge-server.exe --manifest vendor/manifest.toml --verify-executables
```

macOS 使用 `vendor/manifest.darwin-arm64.toml`，Linux ARM64 使用 `vendor/manifest.linux-arm64.toml`，二进制不加 `.exe`。`--verify-executables` 会执行服务端和 FRPS 的 `--version`，需在对应平台运行；所有打包操作只复制明确列出的程序和许可证，不读取部署数据。已有同名服务端包不会被覆盖，重新打包请指定新的 `--output` 目录。
