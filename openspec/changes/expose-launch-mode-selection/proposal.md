## Why

`herdr-mission` 运行时已经支持 `auto` 与 `manual` 两种启动模式，但主要入口与持久化语义仍然分裂：CLI 与 action 已能传入模式，控制中心的新建表单却不能选择；Mission 本身也没有保存模式，导致 `resume`、`start-role` 和角色 Skill 仍可能按 Manual 行为运行。`v0.1.4` 本地外的 canary 还证明 fresh pane 在刚分配后可能短暂返回 `pane_not_found`，Auto 启动仍会中断。

## What Changes

- `herdr-mission new` 未显式传入 `--launch-mode` 时，读取 `~/.config/herdr-mission/config.toml` 的 `[launch].launch_mode`；缺少或无效配置仍安全回退为 `manual`。
- Herdr 的“新建 Team Mission”动作在标题之后询问本次启动模式，接受 `auto`、`manual` 或回车使用配置默认值。
- 单次显式选择优先于全局配置，并将选择作为 `--launch-mode` 传给 Rust CLI。
- Rust 控制中心的“新建 Mission”表单为 Team 布局提供 Auto/Manual 单选项，并把本次选择直接传给启动任务。
- 把解析后的启动模式持久化到 plugin-owned `mission_state`；`status`、`init`、`resume`、`start-role` 和角色 prompt 均读取同一 Mission 状态。
- 增加 `set-launch-mode` 命令，使 Skill 能显式切换已有 Mission 的 Auto/Manual 策略；切换只更新策略，是否补启动缺失角色由显式 `resume` 决定。
- 为配置默认值、显式覆盖和动作输入校验增加回归测试与使用说明。
- 对当前调用刚创建的 Agent pane，在结构化确认仍属于 Mission 工作区且没有 Agent 后，有限重试瞬时 `agent_pane_busy`，避免 Auto 连续启动因 shell 尚未 ready 而中断。
- 对当前调用刚创建但 API 尚未可见的 pane，有限等待 `pane_not_found` 消失；历史 pane 或其它错误仍失败关闭。
- 发布新的预编译 GitHub release，并以本地测试和 release 资产校验完成验收，不再在 `sqbair` 运行 canary。

## Capabilities

### New Capabilities

- `mission-launch-mode-selection`: 规定 Team Mission 新建入口如何选择、继承和覆盖 Auto/Manual 启动模式。

### Modified Capabilities

无。

## Impact

- Rust CLI、TUI 与 runtime：启动模式的创建、持久化、切换、恢复和 fresh pane 就绪重试。
- Herdr 插件动作：`actions/mission-new.sh` 的交互输入与参数传递。
- plugin-owned SQLite：`mission_state` 新增 `launch_mode` 列，旧库幂等迁移并默认 Manual；冻结的 v3 coordination schema 不变。
- Skill、测试与文档：共享 `herdr-mission-team` Skill、CLI/TUI 模式行为和用户配置示例。
- 发布元数据：`Cargo.toml`、`Cargo.lock`、`herdr-plugin.toml`、Git tag 与 GitHub release。
- 不新增依赖，不修改冻结的 v3 coordination 表，不隐式停止或启动已有角色。
