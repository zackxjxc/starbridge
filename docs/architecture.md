# StarBridge 新版架构

2026-10-26 已确认的新客户端交互和会话准入规则见 [客户端交互与会话占用规则](client-experience-and-session-rules.md)。该文档优先于本文中的旧交互或并发目标；下文区分当前实现和待实施目标，不表示新规则已经落地。

## 组件和进程

一期分为中心服务和统一设备节点。中心服务器运行一个 `starbridge-server` 和一个长期运行的 `frps`；每台设备在同一个 Tauri 桌面进程中运行 UI 和 Rust 核心。每台设备同时准备 SSH 登录密钥和本机 SSH 服务，登记实际可用的 `ssh_client`、`ssh_server` 能力。同账户设备可以互相发起连接，设备身份不再固定为某种角色。

端侧可调用本机的 OpenSSH 客户端；本机默认以当前用户权限启动随包携带的 `sshd`，无需管理员安装系统服务；设置 `STARBRIDGE_SSH_PORT` 时改为转发到本机已有的 SSH 服务。StarBridge 不把 SSH 数据协议重新实现为自己的协议，只为本地 SSH 客户端提供一个可用的本地目标地址。

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

已确认的下一版交互为固定 900×600 窗口：同尺寸用户名/密码登录页进入三栏设备列表主界面，服务地址内置且不在 UI 暴露。中栏支持搜索、多选和用途备注，右栏承载单设备 SSH 信息或批量进度与导出；当前窗口尺寸和双栏界面尚未按此调整。

`--headless` 保留本地 HTTP 控制台与原有版本化 REST API，供联调和自动化使用，`--no-browser` 兼容该模式。两种入口复用登录、配置、设备、会话和传输逻辑；`--no-default-features` 可构建不依赖 Tauri 系统库的无界面客户端。

便携版默认将设备数据写入可执行文件旁的 `state/node/`，`--data-dir` 可覆盖；新目录不存在时，若旧 `state/controller/` 含设备身份，会原地复用。旧启动脚本仍使用原数据目录，旧 `--role controller|executor` 参数只用于兼容目录选择，不再限制连接方向。运行时依次查找数据目录、其父目录和可执行文件旁的 `runtime/`。关闭桌面窗口时先停止后台任务，再清理受管子进程，避免窗口退出后留下隧道或 SSH 服务。

## 统一设备与会话准入

会话保留 `initiator_node_id` 和 `target_node_id`，分别决定本次路由的 visitor 和 proxy。创建会话时验证发起设备的身份令牌，不能由中心代选设备。客户端按会话管理 `frpc` 子进程、本地 visitor 端口及 SSH 配置。

当前实现允许同一设备同时出现在不同会话两侧，A→B 与 B→A 可以同时存在、独立断开；重复同向连接会替换旧会话。这是旧的自由混合会话设计，后续按新规格调整：A→B 后 A 仍可连接空闲 C/D，但 B 暂不能发起新连接，其他设备也不能连接已参与会话的 A/B；同向重复请求复用已有会话。协商开始即预占，服务端原子校验和创建，关闭、失败、超时或掉线释放相应占用；A 最后一条出站会话结束后恢复空闲。详细准入、批量取消和验收矩阵以 [新规格](client-experience-and-session-rules.md) 为准。

## 传输适配器

核心层使用统一的 `TransportAdapter` 接口，第一期实现 `InternetFrp`，后续预留 `LanDirect` 和 `VmChannel`。节点、会话、能力和状态模型不绑定某一种网络入口，二期局域网和三期虚拟机间控制需要重新评审后再实现。

## 测试顺序

1. 用 localhost 启动中心 StarBridge 和本地 frps。
2. 启动两个端侧实例，验证登录、登记、心跳和状态同步。
3. 验证 FRP 中继连接和 SSH 业务链路。
4. 加入 XTCP 直连测试，验证直连成功、失败回退、直连禁用和中继禁用。
5. 验证 UI 的直连/中转状态、断线提示和复制全部可连接设备信息。
6. 最后部署到线上域名和 Linux 服务器。
