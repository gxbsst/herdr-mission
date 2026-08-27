## Why

`herdr agent start` 可能在 Agent 进程已经进入目标 pane 后，仍因名称或会话确认未在等待窗口内完成而返回超时。当前 runtime 会把 Mission 标记为 `blocked`，随后重试又把同一 pane 视为忙碌，导致一个实际已运行的 Agent 无法被 Mission 接管。

## What Changes

- 启动返回超时或 `agent_pane_busy` 后，读取目标 pane 的结构化状态并核验 provider、cwd 与 Mission 工作区；命中预期 Agent 时接管现有 pane，而不是重复启动。
- `agent_pane_busy` 后只尝试一次结构化接管；Herdr 0.8.2 无法从结构化状态区分空闲 shell 与无关前台进程时，不在同次调用重复 `agent start`，而是失败关闭并允许显式恢复复用已分配 pane。
- 在 split 或复用 root pane 后、启动 Agent 前先持久化 pane 分配；完成稳定名称注册后再持久化完整运行时身份，避免恢复时重复 split。
- 将 Mission 的三个区域名称固定为 `工作区`、`审查`、`验证`，其中所有 Agent 只在 `工作区` 区域启动和恢复。
- 为迟到成功、正确 Agent 接管、无关占用拒绝、pane 分配恢复和三个区域名称收敛增加回归测试。
- 将插件版本提升到 `0.1.2`，通过现有 tag 工作流发布三平台预编译二进制、校验文件和 commit marker。

## Capabilities

### New Capabilities

- `mission-agent-launch-recovery`: 定义 Agent 启动超时、pane 忙碌和迟到成功时的身份核验、接管与失败关闭行为。
- `mission-region-tabs`: 定义 `工作区`、`审查`、`验证` 三个 Mission 区域及 Agent 只能位于 `工作区` 的命名和布局约束。

### Modified Capabilities

无。

## Impact

- 主要影响 `src/runtime.rs`、`src/herdr.rs`、`src/config.rs` 与对应测试。
- 不新增依赖，不改变 SQLite schema、插件 ID、数据库路径或公开命令名称。
- 发布会产生新的 `v0.1.2` Git tag 和 GitHub Release；现有 `v0.1.1` 保持不可变。
