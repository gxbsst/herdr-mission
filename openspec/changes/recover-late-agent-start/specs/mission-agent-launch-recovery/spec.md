## ADDED Requirements

### Requirement: 迟到启动的 Agent 可被安全接管
当 `herdr agent start` 返回超时或 `agent_pane_busy` 时，系统 MUST 读取目标 pane 及其 tab 的结构化状态；仅当 pane ID、Agent provider、Mission worktree cwd、Agent session 和所属区域均符合预期时，系统 MUST 将该 Agent 接管为当前 Mission 角色。

#### Scenario: 启动超时后目标 Agent 已就绪
- **WHEN** `agent start` 返回超时，且目标 pane 的结构化状态显示预期 provider、Mission worktree cwd、有效 Agent session 和 `工作区` 区域
- **THEN** 系统接管现有 Agent，并继续完成角色启动而不把 Mission 标记为 `blocked`

#### Scenario: provider 先出现而 session 迟到
- **WHEN** `agent start` 返回超时，目标 pane 的 provider、cwd 和区域已匹配但 Agent session 尚未上报
- **THEN** 系统将该状态视为 pending 并继续结构化轮询，不提前失败或重复执行 `agent start`

#### Scenario: pane busy 由预期 Agent 导致
- **WHEN** `agent start` 返回 `agent_pane_busy`，且占用目标 pane 的 Agent 满足全部归属条件
- **THEN** 系统接管该 Agent，而不是重复创建 Agent

#### Scenario: pane busy 但没有可验证 Agent 身份
- **WHEN** `agent start` 返回 `agent_pane_busy`，且结构化 pane 状态无法证明预期 Agent 已就绪
- **THEN** 系统在当前调用中不重复执行 `agent start`，保留已持久化 pane 分配并失败关闭

### Requirement: 接管必须失败关闭
系统 MUST 拒绝接管无关进程、错误 provider、错误 cwd、错误区域、缺失 Agent session 或无法验证结构化状态的 pane，并 MUST 保留可诊断的启动失败结果。

#### Scenario: 无关 Agent 占用目标 pane
- **WHEN** 目标 pane 中的 Agent provider 与角色配置不一致
- **THEN** 系统拒绝接管并将角色启动保持为失败

#### Scenario: Agent 位于错误 worktree
- **WHEN** 目标 pane 中的 Agent cwd 与 Mission worktree 不一致
- **THEN** 系统拒绝接管并将角色启动保持为失败

#### Scenario: Agent 位于错误区域
- **WHEN** 目标 pane 所属 tab 不是 `工作区`
- **THEN** 系统拒绝接管并将角色启动保持为失败

#### Scenario: pane 不是可验证的 Agent 会话
- **WHEN** pane 仅包含无关进程、缺少 Agent session 或结构化状态无法解析
- **THEN** 系统拒绝接管，不从终端文本推断身份

### Requirement: 接管结果使用稳定角色名称
系统接管已运行 Agent 时 MUST 将目标 pane 重命名为 Mission 预期的稳定 Agent 名称，并且 MUST 只在重命名成功后持久化角色的 pane 运行时信息。

#### Scenario: 稳定名称注册成功
- **WHEN** 目标 pane 通过全部接管验证
- **THEN** 系统调用 `herdr agent rename <pane_id> <expected-name>`，持久化同一 pane ID，并允许按稳定名称查询 Agent

#### Scenario: 稳定名称注册失败
- **WHEN** 目标 pane 通过接管验证但 `agent rename` 失败
- **THEN** 系统不把接管记录为成功，并保留角色启动失败状态

### Requirement: 启动恢复保持幂等
系统 MUST 在重复启动或恢复同一 Mission 角色时复用已持久化或已安全接管的 Agent，不得创建重复 Agent。

#### Scenario: 恢复已接管角色
- **WHEN** 同一角色已经接管并持久化目标 pane 后再次执行恢复
- **THEN** 系统复用该 Agent，不再调用第二次 `agent start`

#### Scenario: 恢复已分配但未完成的角色 pane
- **WHEN** 角色已有持久化 `pane_id` 但稳定 Agent 名称仍为空
- **THEN** 系统先验证并尝试接管该 pane 中的 Agent；只有尚无 Agent 时才启动，不再次 split，也不因 `agent_name_taken` 永久 blocked
