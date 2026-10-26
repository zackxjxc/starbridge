# 版本号

项目版本的唯一来源是根目录 `Cargo.toml` 中的 `[workspace.package].version`，当前为 `0.1.0`。
发布时修改这个版本并运行 `cargo check --workspace` 更新 `Cargo.lock`，随后重新构建、打包。
版本号遵循 `主版本.次版本.修订版本`，例如 `0.1.1`；正式发布使用三段数字版本。

- 客户端、服务端和协议包通过 `version.workspace = true` 继承版本。
- 客户端顶栏显示 `v版本号`，登录、设备和启动恢复页面均可见；桌面 IPC 和无界面 HTTP 控制台读取编译时版本。
- 客户端和服务端支持 `--version`；HTTP `/api/health` 返回同一版本。
- Windows 桌面客户端由 Tauri 嵌入版本资源，无界面客户端和服务端由构建脚本嵌入。
  右击 EXE → 属性 → 详细信息，可查看文件版本和产品版本，复制或重命名 EXE 不影响这些属性。
  Windows 数字版本采用四段形式，如 `0.1.0.0`；产品版本字符串为 `0.1.0`。
- Tauri 配置省略 `version`，自动使用 Cargo 包版本；macOS 打包脚本也从工作区读取版本，写入 `.app` 的 `Info.plist`。

验证 Windows 构建：

```powershell
cargo build --release -p starbridge-client -p starbridge-server
Get-Item target/release/starbridge-client.exe, target/release/starbridge-server.exe | ForEach-Object { $_.VersionInfo | Select-Object FileName, FileVersion, ProductVersion }
target/release/starbridge-server.exe --version
```

第三方 `runtime` 目录中的 FRP、OpenSSH 等程序保留其自身版本。
