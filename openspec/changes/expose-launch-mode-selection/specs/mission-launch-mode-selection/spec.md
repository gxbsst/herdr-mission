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

#### Scenario: sqbair 安装新版本
- **WHEN** `sqbair` 已认证访问私有 GitHub 仓库并运行 `herdr plugin install gxbsst/herdr-mission --yes`
- **THEN** 已安装插件报告新版本，release manifest 校验通过，且安装过程使用匹配平台的预编译资产

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
