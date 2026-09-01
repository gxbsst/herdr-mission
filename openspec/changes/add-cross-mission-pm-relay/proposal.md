## Why

Herdr Mission 当前把所有消息限定在单个 Mission 内，因此 PM 无法把一项大工作拆给另一台设备上的独立 Mission；直接放开现有 `pm -> pm` ACL 又缺少目标 Mission、设备身份和可恢复回执，会把跨边界通信伪装成本地角色消息。

## What Changes

- 新增独立的 PM peer relay，使一个 Mission 的 PM 可以向本机或远端设备上的另一个 Mission PM 发送 `delegate`、`context`、`result`、`blocked` 消息。
- 发送端先持久化 outbox；远端经受限 SSH forced-command 接收入站 canonical JSON，提交 durable inbox 后才返回 receipt，网络中断可幂等重试。
- PM `init` 返回未处理 peer inbox；消息不会直接创建跨 Mission Assignment，目标 PM 继续在自己的 Mission 内拆给 Worker、Scout 或 Reviewer。
- 新增 peer 身份与 SSH destination 配置、显式 inbox acknowledge、即时投递和 daemon 重试。
- 普通同 Mission `pm -> pm` 继续 `acl_denied`，非 PM、来源/目标 Mission 或 peer 身份不匹配、相同 ID 不同 payload 等情况全部失败关闭且不产生部分写入。

## Capabilities

### New Capabilities
- `cross-mission-pm-relay`: 定义本机与跨设备 PM peer 消息的身份、持久化、幂等传输、收件箱、确认、唤醒和失败关闭契约。

### Modified Capabilities

无。

## Impact

- 新增 plugin-owned additive SQLite peer 表，不修改冻结 coordination schema v3 的既有表、列或公开语义。
- 新增 peer relay 模块、CLI `peer` 子命令和 `send --target pm --target-mission` 路由；`init` 增加 peer inbox 投影。
- daemon/reconcile 增加 peer outbox 重试与目标 PM best-effort 唤醒。
- 使用系统 `ssh` 和专用 key/forced command，不新增 Rust 依赖；正文只经 stdin 传输，不进入 shell argv。
