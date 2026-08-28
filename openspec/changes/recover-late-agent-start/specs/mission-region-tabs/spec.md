## ADDED Requirements

### Requirement: Mission 使用三个固定区域
系统 MUST 为 Mission 使用三个精确命名的区域：Agent 区域 `工作区`、审查区域 `审查`、验证区域 `验证`。

#### Scenario: 使用默认区域配置
- **WHEN** 系统加载默认 Mission tab 配置
- **THEN** execution、review 和 verification 的值分别精确等于 `工作区`、`审查`、`验证`

#### Scenario: 创建 Mission 区域
- **WHEN** 系统为新 Mission 准备区域布局
- **THEN** workspace 中存在且只复用一组 `工作区`、`审查`、`验证` 区域，不创建重复区域

#### Scenario: Team Mission 使用三区域
- **WHEN** 系统以默认 lanes 布局创建包含 PM、Worker、Scout 和 Reviewer 的 Team Mission
- **THEN** 系统同样建立 `工作区`、`审查`、`验证` 三个区域，所有角色 pane 位于 `工作区`

#### Scenario: 旧配置尝试改名或关闭区域
- **WHEN** 配置文件包含旧的区域名称覆盖或区域开关字段
- **THEN** 系统仍建立且仅使用 `工作区`、`审查`、`验证` 三个固定区域

#### Scenario: 旧 tabs 模式配置
- **WHEN** 配置文件使用兼容值 `tab_mode = "tabs"`
- **THEN** 系统接受该配置但仍使用固定三区域布局，不为各角色创建独立 tab

#### Scenario: 旧 tabs 模式 Mission 已有独立角色 tab
- **WHEN** 恢复旧 Mission 时，已完成角色 pane 位于当前 workspace 的非工作区域 tab
- **THEN** 系统验证角色 Agent 身份后将 pane 移入 `工作区`，并由 Herdr 关闭失去最后一个 pane 的旧 tab

### Requirement: 初始 tab 收敛为工作区
系统复用 workspace 自带的初始 tab 作为 Agent 区域时 MUST 将该 tab 显式重命名为 `工作区`。

#### Scenario: 复用未命名初始 tab
- **WHEN** 新 workspace 已有可复用的初始 tab 且尚未命名为 `工作区`
- **THEN** 系统调用 tab rename 将该 tab 命名为 `工作区`，而不是另建一个 Agent 区域

#### Scenario: 重复确保区域布局
- **WHEN** 系统再次确保已经包含三个固定区域的 Mission 布局
- **THEN** 系统复用现有区域，不重复创建或重命名已正确命名的区域

#### Scenario: 已持久化区域使用旧名称
- **WHEN** Mission 已持久化的 execution、review 或 verification tab 名称不是对应固定名称
- **THEN** 系统验证 tab 仍属于当前 workspace，并分别重命名为 `工作区`、`审查`、`验证`

#### Scenario: 区域创建中途失败后重试
- **WHEN** 系统已成功建立部分固定区域但后续工具启动失败
- **THEN** 系统持久化已建立区域的 tab ID，并在重试时只补齐缺失区域

#### Scenario: 区域创建成功但 ID 尚未持久化
- **WHEN** 固定名称区域已存在于 Mission workspace，但数据库中的对应区域 ID 仍为空
- **THEN** 系统通过结构化 tab list 发现并复用唯一同名区域，不创建重复 tab

#### Scenario: 工作区 root pane 尚未持久化
- **WHEN** 系统通过固定名称发现唯一 `工作区`，但数据库中没有该区域的 root pane ID
- **THEN** 系统通过结构化 pane list 恢复该 tab 中的唯一 pane；若无法唯一确认则失败关闭

#### Scenario: 固定名称区域发现存在歧义
- **WHEN** 区域 ID 为空且 workspace 中存在多个同名固定区域
- **THEN** 系统失败关闭并报告候选 tab ID，不继续创建更多区域

#### Scenario: 修复已知历史字段错位
- **WHEN** worktree/import Mission 的真实路径字段为空，且 `review_tab_id`、`verification_tab_id` 精确呈现已知的绝对路径与 branch 错位形状
- **THEN** bootstrap 在同一事务中恢复 `worktree_path` 与 `branch`，清空错误区域 ID，并允许固定区域发现逻辑继续恢复

#### Scenario: 未知 workspace 损坏形状
- **WHEN** 持久化 workspace 不满足完整的已知错位指纹
- **THEN** 系统不得猜测或自动改写该行

#### Scenario: Mission 区域不属于当前 Herdr session
- **WHEN** 当前 Herdr session 对持久化 workspace 返回结构化 `workspace_not_found`
- **THEN** 系统在任何区域或 Agent 启动副作用前返回不可重试的 `mission_workspace_unavailable`，报告预期 workspace 和底层 Herdr 错误，不跨 session 搜索或重建区域

#### Scenario: 当前 workspace 中缺少持久化区域
- **WHEN** workspace 预检通过但持久化 tab 返回结构化 `tab_not_found`
- **THEN** 系统返回不可重试的 `mission_region_unavailable`，报告预期 workspace、tab、区域名称和底层 Herdr 错误

#### Scenario: 区域工具命令启动失败
- **WHEN** `审查` 或 `验证` 区域已创建，但配置的工具命令无法启动
- **THEN** 系统保留该固定区域的可用 shell 并报告警告，不删除或重复创建区域

### Requirement: Agent 仅属于工作区域
系统 MUST 只在 `工作区` 区域启动、查询或恢复 Mission Agent；`审查` 与 `验证` MUST 保留为独立工具区域。

#### Scenario: 启动 Mission 角色
- **WHEN** 系统为任何 Mission 角色分配 Agent pane
- **THEN** 该 pane 位于 `工作区` 区域

#### Scenario: 拒绝其他区域中的 Agent
- **WHEN** 启动恢复发现匹配 provider 和 cwd 的 Agent 位于 `审查` 或 `验证`
- **THEN** 系统拒绝将该 Agent 接管为 Mission 角色

#### Scenario: 按需启动验证锚点
- **WHEN** PM 请求通过 `start-role` 启动角色
- **THEN** 系统在 split 前验证 anchor pane 属于当前 Mission workspace、持久化 worktree 和 `工作区` 区域，不匹配时拒绝创建 pane
