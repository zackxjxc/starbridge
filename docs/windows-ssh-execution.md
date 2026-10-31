# Windows SSH 命令的控制台窗口

StarBridge 内置的 Windows OpenSSH 以当前用户运行。仅给最外层 `sshd.exe` 设置
`CREATE_NO_WINDOW` 不足以避免远程命令弹窗：OpenSSH 会分离认证前后会话进程的控制台，
之后的 `cmd.exe` 可能重新创建可见窗口。`ssh -T` 只关闭 PTY 请求，不控制 Windows 窗口。

Windows 客户端通过 `SshdSessionPath` 指定随包提供的 `bin/starbridge-ssh-session.exe`。
这个 GUI 子系统启动器使用 `CREATE_NEW_CONSOLE` 和创建时的 `SW_HIDE` 启动官方
`sshd-session.exe`，让普通 shell 和控制台程序继承隐藏控制台。它保留 OpenSSH 传入的
标准句柄和 CRT 文件描述符表，并等待会话退出、返回原始退出码。启动原程序时保留启动器
作为 `argv[0]`，确保 OpenSSH 认证后的再次执行也经过隐藏启动路径。

认证、公钥校验、主机密钥校验和官方运行时文件不变。正式连接与内置连接测试使用相同路径；
原有 Windows Job Object 继续管理完整子进程树。无需管理员权限、注册表修改或系统服务。
macOS/Linux 继续使用原有启动方式；`STARBRIDGE_SSH_PORT` 指定的外部 SSH 服务由该服务负责窗口行为。

构建 Windows 客户端时执行 `cargo build --locked --release -p starbridge-client`，
同时生成客户端和会话启动器。便携包包含两者，会话启动器位于 `bin/` 子目录；请完整解压，更新时同时替换。
只构建 `--bin starbridge-client` 会缺少启动器，启动内置 SSH 时将明确报错。
手动使用 `cargo run` 前，也需先运行 `cargo build --locked -p starbridge-client --bins`；
仓库的 Windows 批处理入口和 `tools/start-local.ps1` 已包含这一步。

验证命令：

```text
cargo test --locked -p starbridge-client --no-default-features
python tools/test_windows_ssh_console.py
python tools/test-peer-connectivity.py
python -m unittest discover -s tools -p test_package_windows.py
```

控制台回归测试只使用临时密钥、回环 SSH 和独立进程树，检查实际远程进程继承的控制台不可见，
并验证重复命令、参数边界、Unicode 标准输入、标准输出/错误、非零退出码、PTY 和 SFTP 双向传输。
`--baseline` 可检查原版 OpenSSH 的可见控制台行为；该模式会复现黑窗。

本修复针对普通 SSH 后台命令。命令主动调用 `start`、创建新控制台或启动图形界面的行为，
仍由该命令决定；启动器不会查找或关闭用户桌面上的窗口。
