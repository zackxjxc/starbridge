# 虚拟机通信拓扑与多模式规划

讨论日期：2026-10-26。本文记录未来方向和技术评估，不表示已经支持任一虚拟化平台。本轮仅更新需求文档，不实现通道、不安装驱动、不设计 UI。

## 1. 当前方向与需要回答的问题

- 实施顺序分两步：先完成局域网直连，再进入虚拟机模式；两步都不以自动单跳为前置条件。
- 局域网模式继续使用 FRP，LAN 单跳中继暂缓，首版按不包含推进；详见 [局域网需求讨论](lan-mode-and-single-hop-relay.md)。
- 未来希望在虚拟机网卡驱动不全、无 IP 连通性时，仍能通过 vsock、VMCI 或类似通道通信。
- 第二步先实现一台宿主机控制多台虚拟机；虚拟机之间的通信作为完成宿主机通道后的进一步能力讨论。
- 判断虚拟机经宿主机中转是否依赖 LAN 单跳，哪些模型可以复用，以及现在需要保留哪些边界。
- 提前记录普通模式、局域网模式、虚拟机模式和模式说明等多个入口的展示要求，本轮不产出 UI 布局或原型。

结论：**先做局域网直连，再做固定宿主机的一对多虚拟机通信；两者都暂不包含自动单跳。虚拟机间经宿主机转发可以复用部分会话与中继生命周期，但必须在宿主机到 guest 的通道稳定后再决定是否支持。**

## 2. vsock、VMCI 与 Hyper-V Socket 的关系

vsock 是虚拟机套接字接口/地址族，Linux 中表现为 `AF_VSOCK`；VMCI 是 VMware 的虚拟机通信机制，能够作为 Linux vsock 的底层传输之一；virtio-vsock 则是另一类虚拟设备/传输。Hyper-V 还有自己的 Windows 套接字接口。因此它们并非跨平台完全等价的两个开关。

| 平台/机制 | 可依赖的设计基线 | 限制与验证边界 |
| --- | --- | --- |
| Linux AF_VSOCK，含 VMware/virtio 等传输 | 按 CID 和端口连接宿主机/虚拟机服务，无需普通网络地址 | 套接字类型、驱动和可达范围取决于底层传输；CID 是端点地址，不是永久设备身份 |
| VMware VMCI/vSockets | 先按宿主机—虚拟机通信设计 | 官方 vSphere API 明确 5.1 及以后不支持 guest-to-guest VMCI；不能把旧性能论文或某产品能力外推到当前 Workstation/Fusion，必须按产品和版本验证 |
| Hyper-V Socket | Windows 侧使用 AF_HYPERV 与 VM/服务 GUID；Linux guest 使用相应 AF_VSOCK 通道 | 需要平台集成支持；Windows 与 Linux 的寻址和 API 有差异，不承诺通用 guest-to-guest 直连 |
| 特定 QEMU 用户态 vsock 后端 | 可额外提供 sibling VM 通信 | rust-vmm 的 vhost-device-vsock 有此能力，但需要相应 VMM 配置；不能把它视为所有 vsock 环境的默认功能 |

产品的可移植基线应采用固定宿主机作为中心的星形结构，不把任意虚拟机之间原生直连作为首版条件。某平台允许直接 guest-to-guest 时，可以以后作为可选路径优化。

### 2.1 绕开网卡不等于无需驱动

通道能够绕开虚拟网卡驱动、IP、DHCP、DNS 等网络配置问题，仍依赖对应 VMCI、virtio-vsock 或 Hyper-V 内核/集成支持。缺少这些支持时需要离线安装或启用，StarBridge 用户态程序不能自行替代内核驱动。

Guest 内仍需要可运行的 StarBridge agent 与 SSH 服务；安装介质可通过 ISO、共享目录或预装镜像提供，不能假定能联网下载。系统尚未启动或 agent 无法运行不属于首版可控制状态。

首轮原型必须明确宿主 OS、guest OS、hypervisor 产品与版本、虚拟设备及驱动。Windows 宿主机的 VMware VMCI、Hyper-V Socket，与 Linux AF_VSOCK 需要分别验证；macOS 和其他 hypervisor 不因接口名称相似就自动纳入支持范围。

## 3. 先支持宿主机控制多台虚拟机

```text
                   宿主机 H：固定管理端
                    /       |       \
                 通道       通道      通道
                  /         |         \
                VM1        VM2        VM3
```

这里的连线是独立的宿主机通道，无需组成虚拟以太网，也不需要 VM1 能访问 VM2。一条通道建立后可以双向传输数据；业务上先限制由 H 发起控制虚拟机，不等于数据只能单向流动。

建议由 guest agent 向宿主机固定服务登记，或由宿主机通过已知 VM 端点主动握手，具体方向按 provider 验证。H 维护本机已认证 guest 目录，结合心跳与虚拟化平台事件更新状态。无需对所有 CID 扫描，也无需将 mDNS 跨虚拟机通道传播。

虚拟化平台枚举到 VM，只表示存在该 VM，不表示 agent 或 SSH 就绪。界面状态需要区分已发现、通道可用、agent 已认证和 SSH 已验证。

### 3.1 复杂度判断

在固定的 hypervisor 与宿主/guest OS 组合上，业务协调比自动 LAN 跳板简单：H 已知、路径固定、目录由 H 维护。可以复用现有一对多业务会话规则，H 分别连接空闲的 VM1/VM2/VM3。

主要新增成本是 provider 接入、驱动/权限检测、离线分发、guest 注册及重启恢复。跨平台兼容性可能比会话逻辑更难；尚未确定支持矩阵前，不给出“所有平台只需少量代码”的估算。

### 3.2 身份和生命周期

- 把稳定的应用设备身份、hypervisor 的 VM 标识、CID/GUID/端口分别保存。CID/GUID 变化或重新分配后需重新认证，不能把地址复用当作原设备回归。
- guest 关闭、休眠、重启、宿主代理退出时，失效对应通道与会话，释放占用并停止导出旧连接命令。
- 快照回滚、克隆和复制磁盘可能复制 agent 私钥与身份，需要冲突检测和明确重新登记，不静默覆盖旧设备。
- 通道来源的 VM 标识只是认证输入，不能替代应用级设备授权；VMCI/vsock 也不能自动提供我们要求的成员信任。
- 登记和心跳属于管理通道，不占用普通 SSH 业务角色，避免 guest 一注册就永远显示被连接。

## 4. 未来增加虚拟机间通信

按可移植方案，路径可以固定为：

```text
VM1 的业务连接 → 宿主机 H 的受控转发 → VM2 的业务入口
```

这在数据路径上与单跳类似，但 H 是明确且固定的协调者，管理所有已登记 guest。H 可以直接提供本机 guest 列表、检查双方身份和占用、预约转发资源并发布结果，不需要在多个未知中继之间发现和选路。

| 工作 | 自动 LAN 单跳 | 固定宿主机的 VM 间通信 |
| --- | --- | --- |
| 协调者 | 需发现和选择可用 B | 已知宿主机 H |
| 节点目录 | 多来源摘要、跨网段更新和去重 | H 维护本机已登记 guest 目录 |
| 路径 | 判断直连与多个候选中继 | 默认固定 VM1—H—VM2 |
| 节点变化 | 同时处理邻居与多来源路径失效 | 处理 guest 注册、退出和宿主连接失效 |
| 会话规则 | 分布式两端准入、B 资源预约 | H 集中协调；guest 仍执行本地授权与必要确认 |
| 传输适配 | 当前拟复用 FRP/STCP | 仍需各 hypervisor provider 与受控桥接 |

因此，完成 H→多 VM 的稳定通道后，VM 间经 H 转发是可以单独追加的能力，通常比全自动 LAN 单跳更容易协调。这是基于固定拓扑的工程判断，不代表免除端点认证、并发取消、故障回收或平台验证。

业务会话应记录 VM1→VM2，H 的转发不等于两条普通 SSH 会话 VM1→H 和 H→VM2。中继资源与业务角色分开记账。

现有占用规则仍有影响：若 H 正在通过普通业务会话连接 VM1，VM1 处于被连接状态，不能同时再发起到 VM2 的业务会话。若未来需要这种并存行为，必须明确调整策略；不能因新增 VM 模式就默认绕过占用。

首版可只提供 H→guest 业务连接，guest 间通信待需要时增加，暂不扩展跨宿主机或嵌套虚拟化路由。

## 5. 是否值得为了未来 VM 模式先做 LAN 单跳

已实现的单跳可能复用：逻辑双方与中继角色、会话 ID、授权检查、预约/提交/取消、超时、连接代次、资源限额和迟到消息处理。

不能直接复用的主要部分是：LAN mDNS、跨网段邻居介绍、多来源候选选路，以及 FRP 的 IP 端点接入。VM 模式仍需要独立的通道 provider、驱动与 OS 适配、VM 标识映射和离线安装方案。

**建议维持 LAN 单跳暂缓。** 现在只在设计上保留设备、会话和传输边界；未来先实现固定宿主机通道及本地目录。需要 VM 间通信时再提取可复用的中继生命周期。先建设固定宿主机转发，也可以反过来为以后 LAN 单跳提供经验。

不为尚未确定的多种模式预先建设庞大的通用路由系统，也不将“代码里存在 Relay 字段”误认为已经解决所有中转问题。

## 6. FRP 与 VM 通道的边界

“局域网模式继续使用 FRP”已经确认，不代表 VM 模式也必须原样使用 FRP。FRP 官方当前客户端传输配置列出的协议是 TCP/KCP/QUIC/WebSocket/WSS，不能将 CID 填入 serverAddr 就认为已有 vsock/VMCI 支持。

未来可比较两条实现路线，当前不做选型承诺：

1. **VM provider 提供受控字节流**：宿主机本地 SSH 入口经过 VM 通道，guest agent 将获准会话接到回环 sshd。SSH 协议和主机密钥校验继续由 OpenSSH 完成，会话入口负责授权与回收。
2. **FRP 经 VM 通道桥接**：保留 loopback TCP 上的 frpc/frps，在两侧提供 TCP↔VM 通道适配。可复用更多 FRP 管理，但额外有桥接进程/任务、多连接和资源回收成本。FRP 可能开启多条底层连接，适配需正确映射独立流，不能把不同连接无分帧混在同一字节流里。

缺少普通网卡驱动时，guest 的 loopback TCP 通常仍可用；如果整个 TCP/IP 栈不可用，则回环 SSH/FRP 方案也需要另行评估。无论选择哪条路线，都不能暴露一个绕过 StarBridge 会话授权的永久转发入口。

## 7. Rust 与开源组件的复用方向

- Linux AF_VSOCK 可评估 `vsock` / `tokio-vsock`，复用现有 Tokio 运行时。不能据此承诺 Windows VMware 或 Hyper-V API 已被同一个 crate 覆盖。
- Windows Hyper-V 需要适配 AF_HYPERV、VM/服务 GUID 和服务注册，可评估 Windows API 绑定；首次注册/权限要求应与日常客户端运行权限分开处理。
- VMware Windows 端需核实当前产品的 vSockets SDK/驱动/API、打包许可和目标系统支持；不假设 Linux 的 AF_VSOCK 实现可直接复制过去。
- rust-vmm 的 `vhost-device-vsock` 属于 VMM 设备后端，可用于受控 QEMU 环境的原型；它不是能直接安装到任意 VMware/Hyper-V 环境的通用 StarBridge 客户端库。
- SSH、身份安全存储、取消代次和进程管理继续复用项目已有能力，具体可复用量在单平台原型后评估。

## 8. 提前记录的多模式 UI 要求

本节是需求约束，不指定控件、页面布局、视觉样式或导航原型，本轮不修改前端。

| 展示项 | 用户需要理解的内容 |
| --- | --- |
| 普通模式 | 当前依赖账户和中心服务的互联网连接；可沿用内部 Internet 命名 |
| 局域网模式 | 免云账号、组内发现、使用 LAN FRP，首版不包含第三节点中继 |
| 虚拟机模式 | 同一宿主机与 guest 的专用通道，实际支持范围随 hypervisor/OS/驱动变化 |
| 模式说明 | 解释各模式适用场景、账号/网络/驱动要求和限制；属于帮助入口，不是第四种传输协议 |

后续设计必须考虑多个 item：

- 模式入口数量可扩展，不按只有两项写死，也不只在登录表单里放一个难以扩展的开关。
- 普通模式要求登录，不应阻塞其他模式的选择；没有云登录也能看到模式说明和适用条件。
- 模式项可说明可用、未支持或缺少驱动/权限等状态，提供具体原因；不能让用户进入后才发现当前平台完全不支持。
- 当前模式、设备所属上下文和会话实际路径分别表达；“虚拟机模式”和“经宿主机转发”不是同一层概念。
- 切换模式时管理已有会话、待处理任务、凭据和列表，不复用错误的账号/组/宿主机上下文。是否允许多个模式并存留待后续确认。
- 记住上次选择时重新检查环境，失败不静默切换到另一模式或降低认证要求。
- 现有固定窗口尺寸下也要容纳多项和说明，但具体使用何种导航、是否调整尺寸，留待 UI 设计阶段决定。

## 9. 建议的验证顺序

1. 先确定一个宿主 OS、guest OS 和 hypervisor 组合，验证禁用普通虚拟网卡后专用通道仍可传输数据。
2. 实现一台 H 管理两个 guest 的原型，验证注册、身份、并行的独立业务会话和真实 SSH，保持现有占用语义。
3. 验证缺驱动、缺权限、guest 重启/休眠、宿主代理重启、CID 变化及克隆身份冲突，明确可恢复和不支持的状态。
4. 如需要 VM 间通信，再验证 VM1—H—VM2，覆盖双方授权、角色冲突、取消与中继资源回收；不把这一步作为 H→guest 的前置条件。
5. 在支持矩阵明确后评估更多平台和 FRP 桥接路线，再开展多模式 UI 设计。

当前待确认的是目标 hypervisor、宿主/guest 系统版本、可接受的驱动预装/离线安装方式，以及未来是否需要 guest 间业务会话。本文不把这些未知条件变成当前实施任务。

## 10. 参考资料

- [Linux vsock(7) 手册](https://man7.org/linux/man-pages/man7/vsock.7.html)：地址族、CID/端口、底层传输和迁移后的重连。
- [Broadcom VirtualMachineVMCIDevice API](https://developer.broadcom.com/xapis/virtual-infrastructure-json-api/latest/data-structures/VirtualMachineVMCIDevice/)：VMCI 设备与 vSphere guest-to-guest 支持限制。
- [Microsoft Hyper-V Socket 官方说明](https://learn.microsoft.com/en-us/windows-server/virtualization/hyper-v/make-integration-service)：专用通道、平台支持、服务注册以及 Windows/Linux API 差异。
- [rust-vmm vhost-device-vsock](https://github.com/rust-vmm/vhost-device/blob/main/vhost-device-vsock/README.md)：特定 QEMU 后端和 sibling VM 通信的配置能力。
- [tokio-vsock](https://github.com/rust-vsock/tokio-vsock)：异步 vsock 客户端/服务端接口。
- [FRP 客户端配置](https://gofrp.org/en/docs/reference/client-configures/)：现有客户端传输协议与连接配置。
