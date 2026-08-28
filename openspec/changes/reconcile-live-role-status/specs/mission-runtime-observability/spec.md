## ADDED Requirements

### Requirement: Mission 协调 Herdr Agent 实时状态

系统 MUST 通过结构化 Herdr Agent snapshot 与持久化角色绑定的精确身份，把匹配 Agent 的 `idle`、`working`、`blocked`、`done` 或 `unknown` 实时状态原样写入 Mission SQLite。dashboard MUST 把 `working` 和遗留 `running` 都显示为“运行中”。

#### Scenario: 工作中的 Reviewer 被显示为运行中

- **WHEN** Herdr Core 返回某 Reviewer 绑定的精确 `pane_id`、Agent 名称和 `agent_status=working`
- **THEN** 系统把该角色的 `team_roles.health` 更新为 `working`
- **AND** dashboard 将 Reviewer 显示为“运行中”

#### Scenario: 无关 Agent 不得修改 Mission

- **WHEN** Herdr Core 返回的 Agent 不匹配任何已持久化角色绑定
- **THEN** 系统 MUST NOT 修改任何 `team_roles.health`

#### Scenario: 完整 snapshot 标记缺失角色

- **WHEN** 一次完整成功的 Agent snapshot 不包含某个同时绑定了 `pane_id` 和 Agent 名称的角色
- **THEN** 系统把该角色的 health 更新为 `missing`

#### Scenario: 未识别状态拒绝整次同步

- **WHEN** 匹配 Agent 返回规范未定义的 `agent_status`
- **THEN** 系统 MUST NOT 修改任何角色 health
- **AND** 系统把本次 health 同步报告为失败

### Requirement: 状态协调失败不阻断消息投递

系统 MUST 在单次 Rust 协调入口中执行实时状态协调和 outbox delivery。Herdr Agent 查询失败或响应无效时，系统 MUST NOT 写入推测的角色状态，并 MUST 继续尝试本次 delivery。

#### Scenario: Agent 列表暂时不可用

- **WHEN** `herdr agent list` 返回非零状态或无效 JSON
- **THEN** 系统不修改已持久化 health
- **AND** 系统仍执行一次 outbox delivery
- **AND** 结构化结果区分状态协调失败与 delivery 结果

### Requirement: 插件生命周期事件使用 Rust 协调入口

插件 startup、`pane.agent_detected` 与 `pane.agent_status_changed` 事件 MUST 直接调用预编译 `herdr-mission` Rust 二进制的协调命令，不得依赖 Python runtime 或仅转发 delivery 的 Shell 事件脚本。

#### Scenario: Agent 状态事件触发协调

- **WHEN** Herdr 发出 `pane.agent_status_changed`
- **THEN** 插件直接运行一次 Rust `reconcile` 命令
- **AND** 该命令先协调已绑定角色的 health，再执行 outbox delivery

### Requirement: dashboard 统计全部未结束 Assignment

Mission status 与 dashboard 的 `pending_assignments` MUST 统计状态为 `queued` 或 `active` 的 Assignment，并与角色 `init` 返回的未结束 Assignment 口径一致。

#### Scenario: Reviewer 已开始审查

- **WHEN** Mission 没有 `queued` Assignment，但 Reviewer 有一个 `active` review Assignment
- **THEN** Mission status 和 dashboard 的 `pending_assignments` 均为 `1`

#### Scenario: 已完成 Assignment 不计入未结束数

- **WHEN** Assignment 状态为 `completed`、`approved`、`rejected` 或其他终态
- **THEN** 该 Assignment MUST NOT 计入 `pending_assignments`

### Requirement: dashboard 分离 Mission 详情与角色表格

dashboard MUST 把选中 Mission 的元数据与角色列表显示在两个独立区域。Mission 详情 MUST 显示 Mission ID、名称、状态、启动模式、未结束任务数、Profile 和 Generation；角色区域 MUST 使用带表头的表格，并清晰区分角色状态、角色名称、Agent 名称与 Pane ID。

#### Scenario: 查看选中 Mission

- **WHEN** 用户在 dashboard 中选中一个 Mission
- **THEN** `Mission 详情` 区域显示该 Mission 的完整 ID 与运行元数据
- **AND** `角色` 区域以表头和逐行单元格展示每个角色
- **AND** Mission 元数据不得与角色行拼接在同一文本段中

#### Scenario: 窄窗口仍保留核心角色列

- **WHEN** dashboard 宽度不足以显示全部角色列
- **THEN** 角色表格优先保留状态、角色、Agent 和 Pane 列
- **AND** 文本不得覆盖相邻列或边框

### Requirement: dashboard 支持终端原生文本选择

dashboard MUST NOT 捕获鼠标事件，并 MUST 在进入 TUI 时关闭终端 mouse capture，使用户可以用鼠标拖选并复制 Mission ID、Agent 名称、Pane ID 和其他可见文本。
当 dashboard 运行在 Herdr popup 中时，Herdr Core MUST 在 dashboard 未请求鼠标上报时释放宿主 mouse capture，不得仅因 popup 存在而强制捕获。

#### Scenario: 鼠标选择文本

- **WHEN** 用户在 dashboard 中拖动鼠标选择可见文本
- **THEN** 终端执行原生文本选择而不是把鼠标事件交给 dashboard
- **AND** dashboard 的键盘导航与刷新继续正常工作

#### Scenario: 需要鼠标的其他 popup

- **GIVEN** 另一个 popup 子程序启用了终端鼠标上报
- **WHEN** Herdr Core 解析到该上报状态
- **THEN** Herdr 继续捕获宿主鼠标并把鼠标事件转发给该 popup
