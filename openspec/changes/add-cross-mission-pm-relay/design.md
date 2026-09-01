## Context

现有 `send` 只有一个 `mission_id`，角色地址也只有 `pm|worker|scout|reviewer`，所以 domain ACL 正确地拒绝 `pm -> pm`。直接放宽 ACL 既无法表达目标 Mission，也会把远端来源伪装为当前 Mission 的 PM。Herdr 的 `--remote` 只把客户端连接到另一台 Herdr server，不合并两台机器的 Agent 或 Mission 命名空间；跨设备传输必须显式经过网络边界。

当前 coordination schema v3 的 `messages`、`outbox` 和 `context_ledger` 只记录一个 Mission 和角色，且已有精确 shape 测试。本变更必须保留这些表及普通 Team ACL，使用 plugin-owned additive schema 保存跨 Mission provenance、传输状态和回执。传输不能依赖 pane 输出作为真源，也不能因为 PM 唤醒失败而丢失消息。

## Goals / Non-Goals

**Goals:**

- 允许本机不同 Mission PM 直接发送可审计消息。
- 允许不同设备上的 Mission PM 经专用 SSH peer 可靠通信。
- 发送端先持久化、接收端提交后回执、断线后幂等重试。
- PM `init` 始终能从 SQLite 读取未处理 peer inbox，再把工作拆为本地 Assignment。
- 保留精确来源/目标 peer、Mission、PM generation、payload digest 和 reply correlation。
- 普通同 Mission `pm -> pm`、非 PM 调用和身份不匹配全部失败关闭。

**Non-Goals:**

- 不把两台 Herdr server 或 Agent 命名空间合并成一个集群。
- 不创建跨 Mission Assignment，也不允许远端 PM直接控制本地 Worker/Scout/Reviewer。
- 不提供公网服务发现、自动密钥分发、SSH key 生命周期管理或中心 broker。
- 不宣称 prompt 唤醒等于目标 PM 已处理；只有 inbox acknowledge 表达本地处理完成。
- 不修改冻结 coordination schema v3 的既有表、列或普通 Team ACL。

## Decisions

### 1. PM peer relay 是 Team kernel 外的独立能力

新增 `peer` 模块和 CLI，而不是把 `send_allowed(Pm, Pm)` 改为 `true`。普通 `send --target pm` 仍走原 kernel 并保持拒绝；只有同时提供不同的 `--target-mission` 时才进入 typed peer path。该 path 只接受 source role `pm` 和 `delegate|context|result|blocked`，不创建 Assignment。

本机目标 Mission 使用同一 SQLite 事务写入 `direction=local,state=accepted` 的 peer message。远端目标使用 `direction=outbound,state=queued`；接收端写入自己的 `direction=inbound,state=accepted`。三个方向共用稳定 envelope 和 inbox 投影，因此本机与跨设备语义一致。

拒绝复用普通 `messages` 的原因是其 `source_role=pm` 不能证明来源 Mission/peer，会污染现有 Assignment、outbox 和 context revision 语义。

### 2. 使用 additive peer schema，不升 coordination schema v3

新增四个 plugin-owned 表：

- `mission_peer_identity`：单例 `local_peer_id`。
- `mission_peers`：`peer_id -> ssh_destination` 的显式 active 配置。
- `mission_peer_routes`：把 `peer_id + local Mission + remote Mission` 绑定为明确的 inbound、outbound 或 bidirectional 授权；receiver 不接受 envelope 临时指定未配置目标。
- `mission_peer_messages`：stable message ID、direction、source/target peer 与 Mission、source PM generation、kind/body、`in_reply_to`、canonical payload SHA-256、state、attempt/error/receipt、notify 与 handled 时间。

本机 send 在 `BEGIN IMMEDIATE` 内精确验证 source/target Mission 及 PM 当前 generation 后写一行；远端 send 验证 source PM 与 peer 配置后写 outbound。receive 在一个 `BEGIN IMMEDIATE` 内验证 forced peer identity、local peer identity、target Mission/PM、payload digest 和 duplicate shape，再写 inbound。

相同 message ID 与相同 canonical digest 返回 duplicate receipt；相同 ID 与不同 digest 返回 non-retryable conflict，且事务零写入。route 只外键引用本机 local Mission，不把远端 Mission 当作本地外键；历史 message provenance 不依赖 route 或远端 Mission 继续存在。本机 local message 同时要求两个 Mission 在事务内存在。

### 3. canonical JSON 从 stdin 经受限 SSH 传输

`peer send` 根据持久化 outbound 行重建版本化 typed payload bytes，计算 SHA-256，并调用系统 `ssh`。这里的 canonical 指本协议固定 struct 字段顺序和规范序列化，不宣称支持任意 JSON/JCS。SSH argv 只包含固定安全选项和经严格字符/前导符校验的 destination；正文与 envelope 只写 stdin，不经过 shell 或 argv。receiver 把 stdin 限制为 256 KiB、正文限制为 64 KiB，并拒绝未知字段。

每个 peer 使用专用 SSH key。目标设备在 `authorized_keys` 配置 `restrict,command="... peer receive --peer <fixed-source-peer> ..."`；forced command 固定 source peer 身份，receiver 不信任 payload 自报的来源。共享 SSH OS 账号本身不够区分 peer，只有专用 key 到 forced command 的绑定构成远端 peer 身份。

SSH 子进程具有 30 秒总执行期限和有界 stdout/stderr；该期限必须严格短于 60 秒 durable claim lease。stdin 写入与 stdout/stderr 读取并发进行，超时后终止子进程并把同一 stable message ID 退回 retry，避免慢请求仍在运行时被第二个 daemon 重领。

接收端必须在 SQLite commit 后输出 `{status,message_id,payload_sha256}` receipt。发送端只在 exit 0 且 receipt 三项精确匹配时把 outbound 标为 `acknowledged`；响应丢失时重试，接收端返回 duplicate receipt。任何非零退出、格式错误或字段不匹配都保留 retry 状态和可诊断错误。

### 4. durable inbox 与 best-effort 唤醒分离

PM `init` 在普通 inbox 之外返回 `peer_inbox`，只包含 target Mission、state=accepted 且未 handled 的 local/inbound 行。`peer ack` 原子写 `handled_at`；ack 后不再作为未处理消息返回。

接收或本机发送提交后，系统 best-effort 调用目标 PM 的 `herdr agent prompt`，提示其运行 `init`。唤醒成功写 `notified_at`；失败保留消息并由 `reconcile`/daemon 重试。允许重复提示，但不允许重复或丢失 durable inbox；prompt 输出不作为处理回执。

### 5. daemon 同时驱动普通 outbox 和 peer relay

现有单 database daemon lock 保持不变。每轮先后驱动普通 kernel outbox、peer outbound SSH 和未通知 inbox 的 PM wake。peer 单轮报告独立计数，单个 peer 网络失败不会阻止其他消息或普通 outbox；错误写回该 message 的 attempts/last_error。显式 `peer deliver` 提供相同的一次性驱动，便于运维和测试。

## Risks / Trade-offs

- [SSH destination 配错或 key 未绑定 forced command] -> receiver 仍验证 fixed peer/local peer/target Mission/digest；文档要求一 peer 一 key，错误保持 retry。
- [SSH 进程连接或 receiver 长时间悬挂] -> 使用 BatchMode、ConnectTimeout 与 ServerAlive 选项；本轮不实现常驻 socket。
- [进程在远端 commit 后、receipt 到达前退出] -> stable ID 与 digest 使重试返回 duplicate receipt。
- [PM pane 被关闭或尚未启动] -> 唤醒失败不影响 accepted inbox，daemon 后续重试，PM 下次 `init` 仍可见。
- [重复唤醒] -> `notified_at` 减少正常重复；通知不是消息处理真源，因此容忍崩溃窗口内重复。
- [本机 peer identity 变更破坏既有 outbox] -> 已存在消息时禁止直接更改 local identity；迁移需显式清空或完成队列后再配置。
- [peer 表被手工篡改] -> bootstrap/peer write path 校验精确列与关键 CHECK；消息读取和 replay 重新验证 envelope/digest，不接受不一致行。
- [远端 PM 把不可信正文直接当命令] -> PM prompt 明确 peer 消息只是协作输入，必须在本地权限和 scope 下重新拆 Assignment。

## Migration Plan

1. bootstrap 幂等创建并验证 additive peer 表，不改变 schema version 3 或冻结 fixture。
2. 配置每台设备唯一 local peer ID，并添加对端 SSH destination。
3. 为每个 peer 生成专用 SSH key，在目标 `authorized_keys` 安装 restrict + forced receive command。
4. 先用 `peer send`/`peer deliver` 验证双向 receipt，再启用 daemon 自动重试。
5. 回滚旧二进制时 additive 表保留但不会被读取；普通 Team 协调继续工作。重新安装新版本后 queued/retry inbox/outbox 可继续处理。

## Open Questions

无。中心 broker、peer key 自动配置和批量拓扑发现留给后续独立变更。
