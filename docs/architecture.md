# StarBridge 新版架构

## 组件和进程

一期有三个角色：中心服务器、控制端节点和执行端节点。中心服务器运行一个 `starbridge-server` 和一个长期运行的 `frps`；每个端侧节点在同一个 Tauri 桌面进程中运行 UI 和 Rust 核心，并管理自己的 `frpc`。客户端通过 `--role controller` 或 `--role executor` 选择角色；一台电脑可同时运行两种角色，但必须使用独立数据目录。

端侧可调用本机的 OpenSSH 客户端；执行端默认以当前用户权限启动随包携带的 `sshd`，无需管理员安装系统服务；设置 `STARBRIDGE_SSH_PORT` 时改为转发到本机已有的 SSH 服务。StarBridge 不把 SSH 数据协议重新实现为自己的协议，只为本地 SSH 客户端提供一个可用的本地目标地址。

## 控制平面和数据平面

端侧 StarBridge 与中心 StarBridge 通过 HTTPS 请求和 WebSocket 长连接完成登录、设备登记、心跳、设备列表、连接请求和状态通知。StarBridge 之间交换的是身份、策略和会话状态，不承载 SSH 字节流。

端侧 `frpc` 与中心 `frps` 建立 FRP 原生控制连接。两端的 FRP 数据路径先尝试 XTCP 直连；直连失败且策略允许时，数据改走 FRPS 中继。SSH、SFTP 或 SCP 产生的 TCP 字节只经过 FRP 数据路径，不经过中心 StarBridge 的 HTTP 业务接口。

## 公网入口

```text
443   nginx，代理 StarBridge HTTPS 服务和 /admin 管理页面
17851 frps 原生 TCP/TLS 入口
17850 StarBridge 服务的本机开发/回环端口，不作为公网入口
```

生产环境由 nginx 终止 HTTPS，并将 `/` 和 `/admin` 转发给中心 StarBridge 的回环监听地址。frpc 直接连接 `starbridge.zackxjxc.de5.net:17851`；用户界面不要求用户填写这个端口。

## Tauri 客户端和无界面模式

端侧 StarBridge 默认打开 Tauri 2 原生窗口，静态 UI 随可执行文件内嵌，通过 Tauri IPC 调用 Rust 核心，访问中心服务和管理 `frpc`、`sshd` 子进程。桌面模式不监听本地 HTTP 控制台端口，不启动外部浏览器；中心服务器及其管理页面不参与这次客户端迁移。

`--headless` 保留本地 HTTP 控制台与原有版本化 REST API，供联调和自动化使用，`--no-browser` 兼容该模式。两种入口复用登录、配置、设备、会话和传输逻辑；`--no-default-features` 可构建不依赖 Tauri 系统库的无界面客户端。

便携版默认将两种角色的数据分别写入可执行文件旁的 `state/controller/` 和 `state/executor/`，`--data-dir` 可覆盖。运行时依次查找数据目录、其父目录和可执行文件旁的 `runtime/`。关闭桌面窗口时先停止后台任务，再清理受管子进程，避免窗口退出后留下隧道或 SSH 服务。

## 传输适配器

核心层使用统一的 `TransportAdapter` 接口，第一期实现 `InternetFrp`，后续预留 `LanDirect` 和 `VmChannel`。节点、会话、能力和状态模型不绑定某一种网络入口，二期局域网和三期虚拟机间控制需要重新评审后再实现。

## 测试顺序

1. 用 localhost 启动中心 StarBridge 和本地 frps。
2. 启动两个端侧实例，验证登录、登记、心跳和状态同步。
3. 验证 FRP 中继连接和 SSH 业务链路。
4. 加入 XTCP 直连测试，验证直连成功、失败回退、直连禁用和中继禁用。
5. 验证 UI 的直连/中转状态、断线提示和复制全部可用执行节点信息。
6. 最后部署到线上域名和 Linux 服务器。
