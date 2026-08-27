## Why

`herdr-mission` 运行时已经支持 `auto` 与 `manual` 两种启动模式，但 CLI `new` 在未传参数时写死为 `manual`，Herdr 的“新建 Team Mission”动作也没有提供模式选择。这使现有 Auto 能力无法从主要入口使用，并导致 `config.toml` 的 `launch_mode` 对新建命令不生效。

## What Changes

- `herdr-mission new` 未显式传入 `--launch-mode` 时，读取 `~/.config/herdr-mission/config.toml` 的 `[launch].launch_mode`；缺少或无效配置仍安全回退为 `manual`。
- Herdr 的“新建 Team Mission”动作在标题之后询问本次启动模式，接受 `auto`、`manual` 或回车使用配置默认值。
- 单次显式选择优先于全局配置，并将选择作为 `--launch-mode` 传给 Rust CLI。
- 为配置默认值、显式覆盖和动作输入校验增加回归测试与使用说明。
- 对当前调用刚创建的 Agent pane，在结构化确认仍属于 Mission 工作区且没有 Agent 后，有限重试瞬时 `agent_pane_busy`，避免 Auto 连续启动因 shell 尚未 ready 而中断。
- 发布新的预编译 GitHub release，并在 `sqbair` 通过 `herdr plugin install` 安装验证。

## Capabilities

### New Capabilities

- `mission-launch-mode-selection`: 规定 Team Mission 新建入口如何选择、继承和覆盖 Auto/Manual 启动模式。

### Modified Capabilities

无。

## Impact

- Rust CLI 与 runtime：`src/cli.rs` 的 `new` 参数默认值解析，以及 `src/runtime.rs` 的 fresh pane 就绪重试。
- Herdr 插件动作：`actions/mission-new.sh` 的交互输入与参数传递。
- 测试与文档：CLI 配置继承、动作选择行为、用户配置示例。
- 发布元数据：`Cargo.toml`、`Cargo.lock`、`herdr-plugin.toml`、Git tag 与 GitHub release。
- 不新增依赖，不修改 SQLite schema，不改变角色启动与恢复协议。
