# 开发环境、服务器和部署约束

## 本地工作区

当前工作区为 `E:/Work/starbridge`。旧版 wstunnel 验证项目位于 `E:/Work/starbridge_old/StarBridge`，不参与新版本构建。当前仓库是本地 Git 仓库，开发阶段只提交本地，不自动推送远程。

已验证的本机工具版本：

```text
rustc 1.94.1
cargo 1.94.1
node v25.2.1
npm 11.6.2
git 2.56.0.windows.2
```

## 本地测试拓扑

```text
中心 StarBridge： http://localhost:17850
本地 frps：     127.0.0.1:17851
端侧控制台：    127.0.0.1:<动态分配端口>
```

多个端侧实例必须使用不同的状态目录、登录身份、visitor 端口和日志文件。测试脚本应能启动两个或更多独立实例，验证节点登记、直连、中转和断线。

Windows 自动测试启动 frpc 时，可能被 Windows Defender、第三方杀毒软件或企业终端安全策略拦截。遇到进程无法启动、立即退出、端口未监听但配置无误等现象时，先检查安全软件告警和隔离区，再判断是否为代码问题。若确认是拦截，需要请求用户手动放行测试用的 FRP 二进制或加入白名单；不得通过测试脚本自动关闭防护或修改安全策略。

## 线上服务器

服务器信息来自本机 SSH 配置和已完成的只读检查：

```text
SSH alias： oracel_ubuntu
主机名：   oracle-ubuntu-arm-vnic
系统：     Ubuntu ARM64
公网 IP：   168.110.58.38
SSH 用户： ubuntu
域名：     starbridge.zackxjxc.de5.net
```

私钥留在本机 `C:/Users/hudening/.ssh/`，不复制进仓库、发布包或文档。域名没有启用 Cloudflare 小橙云，当前由服务器 nginx 监听 80/443。

## 生产入口和防火墙

```text
https://starbridge.zackxjxc.de5.net/          端侧/用户入口
https://starbridge.zackxjxc.de5.net/admin     管理入口
starbridge.zackxjxc.de5.net:17851             frps 原生入口
```

公网只需要开放 443 和 17851。17850 只绑定 127.0.0.1，供 nginx 回源或本机调试，不应开放到公网。生产环境不要求开放 SSH 22 给终端用户；管理员 SSH 访问按服务器现有安全策略处理。

旧版 `starbridge-relay.service` 已停止并禁用。新服务上线前需要创建新的、名称明确的 systemd 单元，并在确认配置后再启用。

## 部署顺序

1. 在本机完成单元测试、FRP 进程管理测试和双节点集成测试。
2. 使用固定依赖清单生成 Windows 客户端和 Linux ARM64 服务端包。
3. 将中心服务和 frps 部署到 `/opt/starbridge`，先只监听回环地址或受控端口。
4. 配置 nginx 将 HTTPS `/`、`/admin` 转发到 StarBridge，配置证书并测试 WebSocket。
5. 放行 443 和 17851，验证域名登录、节点上线、直连、回退和断线。
6. 观察日志和资源使用后，再启用 systemd 开机启动。

## 固定约束

- 每个中心只运行一个 StarBridge 服务和一个长期 frps。
- 每个端侧默认只运行一个 StarBridge 核心和一个 frpc。
- 用户填写无端口的 HTTPS URL，FRP 地址由服务端配置下发。
- 17850 不是公网服务端口；公网 FRP 端口为 17851。
- 依赖二进制由构建流程锁定和校验，不在用户运行时静默下载。

线上启动中心时，`--frps-bind-addr` 用于设置服务器本地的 FRPS 监听地址，`--frps-addr` 用于设置下发给端侧的公网可达地址。例如服务器直接监听公网 FRP 端口时使用 `--frps-bind-addr 0.0.0.0 --frps-addr 168.110.58.38`。
