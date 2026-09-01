## ADDED Requirements

### Requirement: 只有不同 Mission 的 PM 可以使用 peer relay
系统 MUST 仅允许 source role 为 PM、source Mission 与 target Mission 不同且 kind 为 `delegate|context|result|blocked` 的 peer 消息；普通同 Mission `pm -> pm` 与所有非 PM 调用 MUST 继续失败关闭且零写入。

#### Scenario: 同 Mission PM 自投递仍被拒绝
- **WHEN** PM 使用普通 `send` 或 peer path 把消息发给同一 Mission 的 PM
- **THEN** 系统返回 `acl_denied` 或等价 non-retryable Contract 错误，且不写 peer message、Assignment、普通 Message、outbox 或 ledger

#### Scenario: 远端 PM 只能委托目标 PM
- **WHEN** 合法远端 PM 发送 `delegate` 给另一个 Mission
- **THEN** 目标 PM inbox 收到协作消息，但系统不直接创建 Worker、Scout 或 Reviewer Assignment

#### Scenario: 非 PM 或非法 kind 被拒绝
- **WHEN** Worker、Scout、Reviewer 或未知角色尝试发送 peer 消息，或 PM 使用未授权 kind
- **THEN** 系统在任何持久化前以 non-retryable Contract 错误拒绝

### Requirement: 本机跨 Mission 投递必须原子持久化
当 source 与 target Mission 位于同一数据库时，系统 MUST 在单一 `BEGIN IMMEDIATE` 事务内验证两边当前 PM 身份并写入一条 durable local peer message；目标 PM MUST 能通过 `init` 读取该消息。

#### Scenario: 本机 PM 委托另一个 Mission
- **WHEN** source 与 target Mission 及各自 PM 都存在且身份有效
- **THEN** 系统写入一条带精确 source/target Mission、PM generation、kind、body 和 digest 的 local message，目标 PM `init.peer_inbox` 可见，source PM 普通 inbox 不出现该消息

#### Scenario: 任一 Mission 或 PM 不存在
- **WHEN** source/target Mission 或对应 PM 角色缺失
- **THEN** 系统失败关闭，数据库保持调用前快照

### Requirement: 跨设备发送必须先写 durable outbox
跨设备发送 MUST 在执行 SSH 前持久化 stable message ID、canonical payload digest、目标 peer 与传输状态；网络失败 MUST 保留可重试状态。

#### Scenario: SSH 不可达
- **WHEN** outbound 已提交而 SSH 连接失败
- **THEN** 消息保持 `queued` 或 `retry`，attempt 和无敏感错误被记录，daemon 后续可重试

#### Scenario: 进程在远端提交后丢失响应
- **WHEN** 远端已提交 inbound，但发送端没有收到 receipt
- **THEN** 下次用相同 ID 和 digest 重试，远端返回 duplicate receipt，发送端收敛为 `acknowledged`

### Requirement: SSH receiver 必须绑定 peer 身份并验证 canonical envelope
receiver MUST 使用命令行中由 SSH forced command 固定的 source peer identity，MUST 精确验证已启用 route、protocol、source/target peer、source/target Mission、kind、字段形状、大小上限和 SHA-256，不得信任 envelope 自报身份替代 forced identity。

#### Scenario: forced peer 与 envelope 来源不一致
- **WHEN** `peer receive --peer A` 收到自报 source peer B 的 envelope
- **THEN** receiver 返回 non-retryable identity 错误且不写入 inbox

#### Scenario: payload digest 不匹配
- **WHEN** 正文或任一 canonical 字段与 payload SHA-256 不一致
- **THEN** receiver 失败关闭并保持数据库不变

#### Scenario: target peer 或 Mission 不匹配
- **WHEN** envelope 的 target peer 不是本机 identity，或 target Mission/PM 不存在
- **THEN** receiver 拒绝且零写入

#### Scenario: Mission pair 未建立 route
- **WHEN** source peer 已知但 source Mission 到 target Mission 的 inbound route 未配置或已禁用
- **THEN** receiver 拒绝且零写入，不因目标 Mission 名称存在就自动授权

#### Scenario: envelope 超过边界
- **WHEN** stdin 超过 256 KiB、正文超过 64 KiB或 envelope 含未知字段
- **THEN** receiver 在持久化前拒绝

### Requirement: receive 必须提交后回执并保持幂等
receiver MUST 在 inbound SQLite commit 成功后才返回包含 status、message ID 和 payload digest 的 receipt；相同 ID 的 exact replay MUST 返回 duplicate，相同 ID 的不同 payload MUST 返回 non-retryable conflict。

#### Scenario: 首次 receive 成功
- **WHEN** 合法 envelope 第一次到达
- **THEN** receiver 提交一条 inbound accepted message 后返回 accepted receipt

#### Scenario: exact replay
- **WHEN** 同一 source peer 重放相同 message ID 与 digest
- **THEN** receiver 不新增行并返回 duplicate receipt

#### Scenario: message ID 冲突
- **WHEN** 已存在 message ID 被用于不同 digest、Mission、kind、body 或 reply correlation
- **THEN** receiver 返回 non-retryable conflict，既有行和其余数据库状态不变

### Requirement: PM inbox 与处理确认必须持久化
PM `init` MUST 返回目标 Mission 所有 accepted 且未 handled 的 local/inbound peer 消息及完整无歧义 provenance；`peer ack` MUST 只允许目标 Mission PM确认，并在 reopen 后保持 handled 状态。

#### Scenario: reopen 后仍能读取未处理消息
- **WHEN** receive 成功后进程退出且 PM 尚未 ack
- **THEN** reopen 后 `init.peer_inbox` 仍返回同一 message ID、peer、Mission、generation、kind、body、digest 和 reply correlation

#### Scenario: PM 确认处理
- **WHEN** 目标 PM 对其 inbox 中的 message ID 执行 ack
- **THEN** 系统原子记录 handled 时间，后续 `init` 不再把该消息作为未处理项返回

#### Scenario: 非目标 PM ack
- **WHEN** 其他 Mission 或非 PM 尝试 ack
- **THEN** 系统失败关闭且消息保持 accepted

### Requirement: 唤醒失败不得丢失 peer 消息
系统 MUST 把 pane prompt 仅视为 best-effort 唤醒；唤醒失败或目标 pane 不存在时 MUST 保留 accepted inbox，并由显式 deliver、reconcile 或 daemon 重试。

#### Scenario: 目标 PM pane 被删除
- **WHEN** receive 已提交但 `herdr agent prompt` 返回 target not found
- **THEN** peer message 仍在目标 PM inbox，notification 保持待重试，PM 重新创建/加入后可继续收到提示

#### Scenario: 唤醒成功
- **WHEN** 目标 PM Agent 可寻址且 prompt 成功
- **THEN** 系统记录 notified 时间但不把消息标记为 handled

### Requirement: daemon 必须持续推进 peer relay
现有 per-database daemon MUST 在普通 Team outbox 之外持续重试 peer outbound 和待通知 inbox；单条 peer 失败不得阻止其他消息或普通 outbox。

#### Scenario: 多个 peer 中一个离线
- **WHEN** 一个 peer SSH 失败而另一个 peer 可达
- **THEN** 离线消息保持 retry，可达消息完成 receipt，普通 outbox 仍被驱动

#### Scenario: daemon 重启
- **WHEN** daemon 在 queued/retry 状态后重启
- **THEN** 它从 SQLite 恢复并继续相同 stable message ID 的传输，不创建重复 inbound

### Requirement: peer 配置不得把正文或秘密放入 shell argv
系统 MUST 只允许受限格式的 peer ID 与 SSH destination，MUST 用参数化 process API 调用 SSH，并 MUST 通过 stdin 传递 canonical envelope；系统不得调用 shell 拼接命令或把正文写入 argv。

#### Scenario: destination 试图注入 SSH 参数
- **WHEN** destination 以 `-` 开头、包含空白、控制字符或 shell 元字符
- **THEN** peer 配置在持久化前被拒绝

#### Scenario: 正文包含引号和换行
- **WHEN** 合法消息正文包含引号、换行或类似命令的文本
- **THEN** 正文只作为 JSON stdin 字节传输，不改变 SSH argv 或执行额外命令
