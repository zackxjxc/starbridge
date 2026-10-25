# 已知 bug 清单

本文保留 2026-10-24 的历史复现记录。统一节点版本已修复下述第 1、2、4、5、6 项，以及 frpc 进程退出检查和管理页面字段转义；离线断开会持久保存撤销意图，恢复网络后补发。2026-10-26 两栏客户端实施进一步加入同向复用、原子占用、真实 SSH 验证，以及第 3 项所需的持久可轮换刷新凭据；401 时先尝试刷新，有效刷新凭据不应因中心重启直接丢失。以下原始「修复方向」是历史分析，最终规则以 [客户端交互定稿](client-ui-and-interaction-spec.md) 为准。

2026-10-24 在 macOS（Apple Silicon）上基于提交 `24ea3aa` 复现。测试环境是本机中心加 `frps`，同一台机器上运行一个控制端和一个执行端，策略为 `relay_only`。下面的问题都只需要按 bug 修复，不改变一期设计。`create_session` 不识别发起控制端的问题见 [phase-1-design.md](phase-1-design.md#兼容性) 的当前身份模型，这里不重复。

## 共同原因

第 1 到第 5 条是同一个根源：会话只有在“断开请求成功送达中心”时才会结束。重复建联、中心不可达、登出、控制端崩溃这几种情况都走不到这一步，会话会一直停在 `connected`，执行端心跳又会按这个会话继续拉起 frpc。

## 1. 不断开就再次建联，旧会话会复活

- 位置：`client/src/lib.rs` 的 `connect`
- 复现：控制端对同一执行端连续两次 `POST /api/v1/connect`，然后断开。
- 证据：
  - 中心上两个会话 `d72c3717`、`a8abb21a` 都是 `connected relay`。
  - 断开 `a8abb21a` 后，执行端在下一次心跳切回 `d72c3717`，并为它重新启动 frpc（执行端 frpc 进程数为 1）。
- 原因：`connect` 发起新会话前没有关闭本地已有的会话，也没有通知中心。
- 修复方向：建联前先对当前会话执行和断开相同的清理，即通知中心关闭并停止 frpc。

## 2. 中心不可达时断开失败，本地 frpc 不停

- 位置：`client/src/lib.rs` 的 `disconnect`
- 复现：建联后停止中心，再调用 `DELETE /api/v1/disconnect`。
- 证据：接口返回 `中心服务连接失败：error sending request ...`；控制端仍是 `frpc_running ... connected relay`，frpc 进程仍在运行。
- 原因：`DELETE` 请求失败时 `?` 提前返回，后面的 `stop_frpc` 和清除会话都没有执行。
- 修复方向：先在本地停止 frpc、清除会话，中心请求失败只作为提示返回给用户。

## 3. 中心重启后端侧永久掉线

- 位置：`server/src/lib.rs` 的 `PersistedState`
- 复现：两端登录后重启中心，等待两次心跳。
- 证据：两端都停在 `heartbeat_failed`，`last_error` 为 `HTTP status client error (401 Unauthorized) for url (.../api/v1/devices/heartbeat)`，只能手动重新登录。
- 原因：`tokens` 只保存在内存里，`PersistedState` 只持久化 `accounts`、`devices` 和 `sessions`。
- 修复方向：把令牌和过期时间一起持久化，载入时丢弃已过期的令牌。

## 4. 有会话时登出，会话和执行端 frpc 残留

- 位置：`client/src/lib.rs` 的 `logout`
- 复现：建联后在控制端调用 `POST /api/v1/logout`，等待一次心跳。
- 证据：登出 12 秒后，中心上的会话 `91d12192` 仍是 `connected relay`，执行端 frpc 进程数为 1。
- 原因：`logout` 只停止本地 frpc，没有通知中心关闭会话。
- 修复方向：登出前先关闭远端会话。

## 5. 控制端崩溃后会话永不结束

- 位置：`server/src/lib.rs` 的 `assigned_sessions`
- 复现：建联后用 `kill -9` 结束控制端及其 frpc，等待 45 秒。
- 证据：中心已经把控制端显示为离线，会话 `21fbe6d5` 仍是 `connected relay`。
- 原因：会话创建时写入了 `expires_at = created_at + 600`（`server/src/lib.rs:385`），但服务端没有任何地方检查它，只检查令牌过期（`server/src/lib.rs:587`）。
- 修复方向：`assigned_sessions` 过滤掉控制端已离线的会话。不能直接按 `expires_at` 过滤：执行端看到会话不再分配就会停止 frpc，10 分钟的过期时间会切断正常的长时间 SSH 会话。`expires_at` 只适合用于从未进入 `connected` 的会话，或者改成由控制端心跳续期。

## 6. 执行端有多个活动会话时选错

- 位置：`client/src/lib.rs` 的 `heartbeat_once`（`assigned.first()`）
- 复现：第 4 条留下的会话 `91d12192` 仍处于活动状态时，新建会话 `21fbe6d5`。
- 证据：执行端 `routes/` 目录下只有 `91d12192`、`a8abb21a`、`d72c3717` 的配置，没有 `21fbe6d5`，执行端没有为新会话启动 proxy，这次连接不可能成功。
- 原因：中心从 `HashMap` 取出会话，返回顺序不确定，执行端只取第一个。
- 修复方向：执行端取 `created_at` 最大的会话，或者中心按创建时间排序后返回。第 1 到第 5 条修好后这种情况很难再出现，但这里本身也应该修。

## 次要问题

### 执行端不监督 frpc 进程

- 证据：测试中手动结束执行端的 frpc 后，执行端没有发现，也没有重启，状态一直显示 `frpc_running ... connected relay`，实际 frpc 进程数为 0。
- 说明：只验证了被外部结束的情况，没有在自然条件下复现 frpc 自行崩溃。
- 修复方向：心跳时对 frpc 子进程调用 `try_wait`，发现退出就按当前会话重启，或者把会话上报为失败。

### UI 拼接 HTML 时没有转义

- 位置：`client-ui/index.html`、`admin-ui/index.html`
- 说明：两个页面都用 `innerHTML` 直接拼接 `display_name`、`ssh_alias`、`last_error` 等字段。同一账户下的设备可以把 HTML 或脚本注入到其他端侧的本地控制台。影响范围限于同一账户，没有实际验证。
- 修复方向：插入前做 HTML 转义，或改用 `textContent`。

## 修复后的回归

第 1、2、4、6 条和 frpc 监督改的是两个平台共用的端侧逻辑，修复后需要在 macOS 和 Windows 上各跑一遍端到端测试（Windows 用 `deploy/windows/test-ssh.ps1`）。
