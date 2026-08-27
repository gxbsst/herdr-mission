## ADDED Requirements

### Requirement: 新建命令继承全局启动模式
系统 SHALL 在 `herdr-mission new` 未提供 `--launch-mode` 时读取 `LaunchConfig` 中的 `launch.launch_mode`，并在配置缺失或无效时使用 `manual`。

#### Scenario: 配置 Auto 且命令未覆盖
- **WHEN** 全局配置的 `launch_mode` 为 `auto`，且用户执行 `new` 时未提供 `--launch-mode`
- **THEN** 系统以 Auto 模式启动该 Mission

#### Scenario: 配置 Manual 且命令未覆盖
- **WHEN** 全局配置的 `launch_mode` 为 `manual`，且用户执行 `new` 时未提供 `--launch-mode`
- **THEN** 系统仅启动 PM，并让其他角色等待按需启动

#### Scenario: 配置不可用
- **WHEN** 配置文件缺失、无法读取或内容无效，且用户未提供 `--launch-mode`
- **THEN** 系统使用内置 `manual` 默认值

### Requirement: 显式启动模式覆盖全局配置
系统 MUST 让合法的 `--launch-mode auto|manual` 覆盖全局配置，且覆盖只作用于当前新建 Mission。

#### Scenario: 显式 Auto 覆盖 Manual 配置
- **WHEN** 全局配置为 `manual`，用户执行 `new --launch-mode auto`
- **THEN** 系统以 Auto 模式启动当前 Mission

#### Scenario: 显式 Manual 覆盖 Auto 配置
- **WHEN** 全局配置为 `auto`，用户执行 `new --launch-mode manual`
- **THEN** 系统仅为当前 Mission 启动 PM

### Requirement: Herdr 动作提供启动模式选择
Herdr 的“新建 Team Mission”动作 SHALL 在获取标题后接受 `auto`、`manual` 或空输入，并把合法的单次选择传给 Rust CLI。

#### Scenario: 动作选择 Auto
- **WHEN** 用户在启动模式提示中输入 `auto`
- **THEN** 动作使用 `--launch-mode auto` 创建 Mission

#### Scenario: 动作选择 Manual
- **WHEN** 用户在启动模式提示中输入 `manual`
- **THEN** 动作使用 `--launch-mode manual` 创建 Mission

#### Scenario: 动作使用配置默认值
- **WHEN** 用户在启动模式提示中直接回车
- **THEN** 动作不传 `--launch-mode`，由 Rust CLI 继承全局配置

#### Scenario: 动作拒绝未知值
- **WHEN** 用户输入 `auto`、`manual` 以外的非空值
- **THEN** 动作在创建 Mission 前返回错误，且不得调用 Rust CLI

### Requirement: 发布产物保持预编译安装
发布流程 SHALL 为新版本生成受 commit 与 SHA-256 约束的预编译二进制，安装端 SHALL 能通过 GitHub 插件安装链路使用该二进制而无需 Python 或本地 Cargo 构建。

#### Scenario: GitHub 生成新版本预编译资产
- **WHEN** 发布新的不可变版本 tag
- **THEN** GitHub Release 包含三个目标平台二进制、`SHA256SUMS` 与绑定发布 commit 的 `COMMIT`，安装脚本可在已认证环境直接下载并校验匹配资产而无需 Python 或本地 Cargo 构建

### Requirement: Auto 启动等待 fresh pane 就绪
系统 SHALL 在本次启动调用刚创建的 pane 瞬时返回 `agent_pane_busy` 时，先结构化验证其 Mission 归属，再有限重试同一角色的 `agent start`；系统 MUST NOT 对归属不明或此前已持久化的空 pane 重复启动。

#### Scenario: fresh pane 的 shell 延迟就绪
- **WHEN** Auto 启动刚创建的 pane 第一次返回 `agent_pane_busy`，且 pane ID、workspace、cwd、工作区 tab 全部匹配且没有 Agent/session
- **THEN** 系统在有限等待后重试，并在 shell 就绪时继续启动剩余角色

#### Scenario: fresh pane 已出现 Agent 身份
- **WHEN** `agent_pane_busy` 后结构化状态显示预期 provider 已在目标 pane 中启动但 session 尚未上报
- **THEN** 系统只轮询并接管该 Agent，不得再次执行 `agent start`

#### Scenario: 历史 pane 仍为空且 busy
- **WHEN** 恢复此前已持久化的角色 pane，`agent start` 返回 `agent_pane_busy` 且没有可验证 Agent 身份
- **THEN** 系统保持失败关闭，不得循环执行 `agent start`

#### Scenario: pane 归属不匹配
- **WHEN** busy pane 的 workspace、cwd、工作区 tab ID 或名称与 Mission 不匹配
- **THEN** 系统立即拒绝重试并把 Mission 保持为 blocked

### Requirement: 控制中心可选择启动模式
系统 SHALL 在 Team Mission 新建表单中显示 Auto/Manual 单选项，并把本次选择传给创建任务；后台创建任务 MUST NOT 重新读取全局配置覆盖该选择。

#### Scenario: TUI 选择 Auto
- **WHEN** 用户在 Team 新建表单选择 Auto 并创建 Mission
- **THEN** Mission 持久化 `auto`，并以 Auto 启动缺失角色

#### Scenario: TUI 选择 Manual
- **WHEN** 用户在 Team 新建表单选择 Manual 并创建 Mission
- **THEN** Mission 持久化 `manual`，且首次只启动 PM

### Requirement: Mission 持久化启动模式
系统 SHALL 在 plugin-owned `mission_state` 中持久化 `launch_mode`，旧数据库缺少该列时 MUST 幂等迁移并将已有 Mission 视为 Manual。

#### Scenario: 显式 Auto 创建后配置变化
- **WHEN** Mission 以显式 Auto 创建，随后全局配置变为 Manual
- **THEN** `status`、`init`、`resume` 与 `start-role` 仍读取该 Mission 的 Auto 模式

#### Scenario: 旧库升级
- **WHEN** bootstrap 打开没有 `launch_mode` 列的 Rust-owned 数据库
- **THEN** 系统补充该列且保留全部已有 Mission、角色和工作区数据

### Requirement: Skill 可切换已有 Mission 模式
系统 SHALL 提供 `set-launch-mode` 命令原子切换已有 Mission 的 Auto/Manual 策略，并在共享 `herdr-mission-team` Skill 中说明真源、命令和副作用边界。

#### Scenario: 切换为 Auto
- **WHEN** PM 或用户对已有 Mission 运行 `set-launch-mode ... auto`
- **THEN** 后续 `init` 返回 `auto`，但命令本身不隐式创建 pane；需要补启动时显式运行 `resume`

#### Scenario: 非法模式
- **WHEN** 切换命令收到 Auto/Manual 之外的值
- **THEN** 系统失败关闭且不得修改 Mission 状态

### Requirement: Mission pane 延迟可见
系统 SHALL 对数据库已记录或本次启动刚分配的精确 pane ID 有限只读重试短暂 `pane_not_found`，并在 pane 可见后执行完整 Mission 归属验证；系统 MUST 只对本次 fresh pane 重复 `agent start`。

#### Scenario: fresh pane 首次不可见随后出现
- **WHEN** fresh pane 在 busy 恢复路径第一次 `pane get` 返回 `pane_not_found`，随后以匹配 workspace、cwd 与“工作区”tab 出现
- **THEN** 系统继续 Auto 启动且不得跳过结构化归属验证

#### Scenario: pane 持续不存在
- **WHEN** fresh pane 在有限等待结束后仍返回 `pane_not_found`
- **THEN** Mission 保持 blocked，系统不得创建未记录的替代 pane

#### Scenario: 恢复历史未完成 pane
- **WHEN** 已持久化但未完成启动的 pane 首次 `pane get` 返回 `pane_not_found`，随后以匹配归属出现且为空
- **THEN** 系统只执行一次 `agent start`，不得使用 fresh pane 的重复启动权限
