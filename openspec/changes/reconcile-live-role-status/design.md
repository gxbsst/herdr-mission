## Context

Mission dashboard 只读取 `team_roles.health` 和 `assignments`，不会查询 Herdr Core。角色启动或手动 `join` 时，Rust runtime 把 health 初始化为 `idle`；之后 `pane.agent_status_changed` 触发的 `events/reconcile-delivery.sh` 仅调用 `herdr-mission deliver --json`，没有消费 Core 的实时 `agent_status`。因此 Core 与 SQLite 会长期分裂。

当前 `read_role_context` 把 `queued` 和 `active` 都视为待处理 Assignment，但 `read_mission_status` 与 dashboard overview 只统计 `queued`，同一二进制内部也存在口径不一致。

约束：SQLite 仍是 dashboard 的事实来源；不修改 v3 schema；不新增依赖；Core 查询失败不能阻断原有 event-driven delivery。

## Goals / Non-Goals

**Goals:**

- 用 Rust 协调入口把 Herdr Core 的实时 Agent 状态写入绑定角色的 `team_roles.health`。
- 保持精确身份匹配和 fail-closed 写入边界，不用 pane 标题或模糊字符串识别角色。
- 让 startup 和 Agent 生命周期事件先协调 health，再执行一次现有 outbox 投递。
- 让 Mission status 与 dashboard 对未结束 Assignment 统一统计 `queued + active`。

**Non-Goals:**

- 不把 Herdr Core 变成 Mission Assignment 或消息的事实来源。
- 不修改 Mission schema、角色绑定、Assignment 状态机或 launch generation。
- 不因单次 Core 查询缺失就断言 pane 已关闭或自动重启角色。
- 不恢复 daemon，也不引入轮询进程。

## Decisions

### 1. 新增单次 Rust `reconcile` 命令

`herdr-mission reconcile --json [--database=...]` 依次执行：

1. 取得 Mission SQLite 的 immediate transaction 写保留，串行化并发生命周期事件与角色重绑。
2. 在该临界区内通过现有可注入 `ProcessRunner` 调用并解析结构化 `herdr agent list`。
3. 将 Agent snapshot 与事务中读取的 `pane_id` 和 `terminal_id`（当前存放 Agent 名称）精确匹配，仅更新匹配角色的 `health` 与 `updated_at`。
4. 无论 health 查询是否可用，都执行一次现有 `kernel_deliver`。

选择单一命令而不是让 dashboard 临时查询 Core，是为了保持 dashboard 只读、SQLite 可审计，并让 CLI、dashboard 和事件消费同一状态。

### 2. 保留 Herdr 的细粒度状态词汇

- `idle`、`working`、`blocked`、`done`、`unknown` 原样持久化
- 完整成功 snapshot 中不存在的已绑定角色 → `missing`
- 未绑定角色 → 保留原值
- 未识别状态 → 整次 health 同步失败且不写数据库

SQLite 恢复层已经把这些细粒度 health 归一化为既有 `RoleState`，无需扩展状态机。dashboard 把 `working` 与遗留 `running` 都显示为“运行中”，并为 `blocked`、`done`、`unknown` 和 `missing` 提供明确标签。只有一次结构完整、成功返回的 Agent 列表才有资格把已绑定但缺失的角色写成 `missing`。

### 3. health 协调失败不能吞掉 delivery

Core 命令失败、JSON 异常或字段缺失时，Rust 命令不写任何 health，并记录结构化错误；随后仍执行 outbox delivery。组合结果会明确区分 health reconciliation 与 delivery 结果，防止观测同步退化为消息丢失条件。

### 4. 插件事件直接执行预编译 Rust 二进制

`herdr-plugin.toml` 的 startup 与两个 Agent 事件使用 `./target/release/herdr-mission reconcile --json`。删除只负责 `exec` Rust 的 `events/reconcile-delivery.sh`，避免把 Shell 启动器误认为另一套 runtime，也减少一个事件入口。由于 plugin-root 相对程序解析从 Herdr `0.8.0` 起才受支持，manifest 的最低 Herdr 版本同步提升到 `0.8.0`。

### 5. `pending_assignments` 统一表示未结束任务

`read_mission_status` 和 `read_mission_overviews` 的查询改为 `state IN ('queued', 'active')`，与角色 `init` 已有语义一致。字段名和 schema 不变，避免无必要迁移。

### 6. dashboard 分离 Mission 信息与角色表格

选中项详情拆成两个独立区域：上方 `Mission 详情` 显示名称、完整 Mission ID、状态、启动模式、未结束任务、Profile、Generation 和创建时间；下方 `角色` 使用带表头的表格展示状态、角色、Provider/Model、Agent 名称和 Pane ID。窄窗口优先保留状态、角色、Agent 与 Pane，并增加详情区高度，避免元数据和角色行混成一段文本或相互遮挡。

dashboard 不注册鼠标事件，并在进入 TUI 后显式发送 `DisableMouseCapture`。Herdr Core 必须让 popup 的宿主捕获状态服从 popup 子程序的鼠标上报状态，不能仅因 popup 存在就强制捕获；这样外层终端才能处理普通拖选与复制。需要鼠标的其他 popup 在启用鼠标上报后仍由 Herdr 转发事件。键盘仍是 dashboard 的唯一交互输入。

## Risks / Trade-offs

- [Core 在 Agent 状态切换后短暂返回旧 snapshot] → 生命周期事件会再次触发；精确匹配且幂等更新，不增加轮询。
- [Core 查询失败导致 dashboard 暂时保留旧 health] → 保留最后可信值并报告 reconciliation unavailable，不伪造新状态，delivery 继续。
- [旧绑定只有 pane 或 Agent 名称之一] → 只对同时具备 pane 与 Agent 名称的绑定做实时协调；未绑定角色保持不变。
- [直接 Rust event command 在安装期间不存在] → 插件 build 仍先下载并校验二进制；发布安装验证覆盖 startup/event manifest。

## Migration Plan

1. 发布包含新命令与 manifest 的预编译版本。
2. 重新执行 `herdr plugin install gxbsst/herdr-mission --yes`，原 SQLite 不迁移。
3. 安装后运行一次 `herdr-mission reconcile --json`，核对现有 Mission 的实时角色 health 和未结束 Assignment 计数。
4. 回滚时重新安装 `v0.1.5` 源提交；数据库 schema 未变化。

## Open Questions

无。
