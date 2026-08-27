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
