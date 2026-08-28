## Why

Herdr Core 已把活跃 Reviewer 标记为 `working`，但 Mission SQLite 仍保留 `idle`，导致 dashboard 把正在工作的角色显示为“空闲”。同一界面只统计 `queued` Assignment，又把已经开始处理的 `active` Assignment 排除在 `pending` 之外，因此会同时出现“Reviewer 空闲”和“pending 0”的误导状态。

## What Changes

- 新增 Rust 运行态协调入口：读取 Herdr Core 的 Agent 列表，把已绑定角色的实时状态映射并持久化到 Mission SQLite，然后执行现有消息投递。
- 让 `pane.agent_detected`、`pane.agent_status_changed` 和插件启动事件调用同一个 Rust 协调入口，不再依赖只转发 `deliver` 的 Shell 包装。
- 统一 Mission 状态与 dashboard 的未结束 Assignment 口径，统计 `queued` 和 `active`。
- 重构 dashboard 详情区：Mission 元数据与角色列表分区展示，角色使用带表头的表格，并保留终端原生鼠标选中文本能力。
- 为实时状态映射、缺失/异常 Core 输出、事件协调、dashboard 角色状态与 Assignment 统计增加回归测试。

## Capabilities

### New Capabilities

- `mission-runtime-observability`: 规定 Mission 如何从 Herdr Core 协调角色实时状态，并在 dashboard 中展示角色活跃度和未结束 Assignment。

### Modified Capabilities

无。

## Impact

- 受影响代码：Rust CLI、Herdr adapter、Mission SQLite 状态写入、dashboard overview 查询、插件 manifest 和事件入口。
- 不新增依赖，不修改 schema，不改变 Mission ID、Assignment ID 或现有数据库路径。
- 发布后需要重新安装 GitHub 插件，使事件声明和预编译 Rust 二进制同时更新。
