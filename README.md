# StarBridge

StarBridge 是一个基于 FRP 的跨网络设备连接工具。客户端使用本地浏览器打开控制台，中心服务器通过 HTTPS 提供账户、设备和连接控制服务，FRP 负责直连协商和中继回退。

一期先实现中心服务器远程组网：优先尝试 FRP XTCP 直连，失败后按本次连接策略回退到 FRPS 中继。控制台显示当前是“直连”还是“中转”，并允许用户在建联前选择是否允许直连。设备属性中可以设置“本机只允许直连”。

## 文档

- [一期详细设计](docs/phase-1-design.md)
- [架构总览](docs/architecture.md)
- [FRP/OpenSSH 来源与打包](docs/dependencies-and-packaging.md)
- [二期、三期和未来扩展路线](docs/future-roadmap.md)
- [开发环境、服务器和部署约束](docs/development-environment.md)

## 开发阶段

开发阶段中心服务器运行在本机，客户端使用 `localhost` 连接。通过本地浏览器访问客户端控制台，不制作 Electron 版本。依赖二进制在构建前按锁定版本下载、校验并复制进发布目录，不从运行中的用户电脑临时下载。

## 线上地址和端口

线上部署阶段使用以下地址：

```text
https://starbridge.zackxjxc.de5.net
https://starbridge.zackxjxc.de5.net/admin
```

公网 HTTPS 使用 443 端口，FRP 原生入口使用 17851 端口。用户只填写 HTTPS 地址，FRP 地址和端口由 StarBridge 在登录后自动配置。17850 仅作为服务器本机开发或回环服务端口，不作为公网入口。

## 目录

```text
client/       端侧 StarBridge 核心服务
server/       中心 StarBridge 服务
client-ui/    端侧本地浏览器 UI
admin-ui/     中心服务器管理 UI
vendor/       已锁定并校验的外部二进制和许可证
deploy/       本地和 Linux 部署配置
docs/         架构、打包和开发文档
```

旧版 wstunnel 演示项目保存在工作区旁边的 `D:/Work/starbridge-reference-20261022`，只作为参考，不参与新版本构建。
