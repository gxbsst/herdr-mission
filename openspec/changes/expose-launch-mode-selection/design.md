## Context

运行时的 `LaunchMode` 已经能够在 `auto` 时一次启动 PM、Worker、Scout、Reviewer，在 `manual` 时仅启动 PM。当前问题不是运行时缺少能力，而是入口语义分裂：`resume` 和 TUI 会读取 `LaunchConfig`，`new` 却先写死 `Manual`；插件动作只询问标题，也无法生成显式覆盖参数。

本变更横跨 Rust CLI、交互式 shell 动作、测试和发布元数据，但不涉及 SQLite、Agent 启动协议或 Herdr Core。

## Goals / Non-Goals

**Goals:**

- 让 CLI `new`、TUI 和 `resume` 对全局 `launch_mode` 使用一致语义。
- 让插件“新建 Team Mission”动作可以为当前任务选择 `auto` 或 `manual`。
- 明确优先级：单次显式参数高于全局配置，全局配置缺失或无效时为 `manual`。
- 保持错误输入失败关闭，不能在用户输入错误模式时静默创建 Mission。

**Non-Goals:**

- 不把全局默认从 `manual` 改为 `auto`。
- 不改变 Auto/Manual 在运行时启动哪些角色。
- 不增加 Herdr manifest 参数系统或依赖新的交互工具。
- 不修改已有 Mission 的启动状态或自动恢复角色。

## Decisions

### 1. CLI 用可选覆盖值表达参数是否出现

`run_new` 解析期间把启动模式保存为 `Option<LaunchMode>`。只有出现合法的 `--launch-mode auto|manual` 才写入覆盖值；参数缺失时，在构造 `LaunchOptions` 前从一次加载的 `LaunchConfig` 取默认值。

这样可以区分“用户显式选择 manual”和“没有选择，应继承配置”，同时避免在 shell 中重复实现 TOML 解析。没有采用新增 `default` 参数值，因为公开 CLI 已经能用“参数缺失”准确表达继承。

### 2. 插件动作以回车表示继承配置

`mission-new.sh` 在标题后询问 `启动模式 [auto/manual，回车使用全局配置]`。输入 `auto` 或 `manual` 时追加对应 CLI 参数；回车时不传 `--launch-mode`，由 Rust CLI 读取配置。其他输入在创建 Mission 前返回非零并给出中文错误。

没有把 Auto 设为动作默认值，因为升级后突然启动四个 Agent 会增加资源消耗并改变现有用户行为。也没有直接在 manifest 中增加选择字段，因为当前 Herdr action 契约是执行交互式命令，现有脚本已经承担标题输入。

### 3. 用纯函数与进程级脚本测试覆盖优先级

Rust 单元测试覆盖显式值优先、配置值继承和内置 Manual 回退。动作集成测试使用临时插件根和记录参数的假二进制，验证 `auto`、`manual`、回车及非法输入，不创建真实 Mission 或 Herdr pane。

该组合直接锁定本次两个缺口；现有 runtime 测试继续证明 Auto/Manual 的角色启动差异，无需重复构造完整 Herdr 模拟环境。

### 4. 以 v0.1.3 发布并复用现有预编译链路

版本同步提升到 `0.1.3`。推送 `v0.1.3` 后由现有 GitHub Actions 构建三个目标平台，生成 `SHA256SUMS` 与 `COMMIT`。`sqbair` 继续通过已认证的 `herdr plugin install gxbsst/herdr-mission --yes` 下载预编译产物，不依赖 Python 或现场 Cargo 编译。

## Risks / Trade-offs

- [交互动作多一次输入] -> 回车直接继承配置，保留最短操作路径。
- [用户误拼模式] -> 在调用二进制前失败关闭并列出仅接受的值。
- [无效配置被静默回退] -> 延续 `LaunchConfig::load` 现有兼容语义；`doctor` 行为不在本次扩大。
- [显式参数与配置读取顺序混淆] -> 用单一解析优先级函数和测试固定“显式覆盖 > 配置 > Manual”。
- [私有仓库 release 下载依赖认证] -> 发布端和安装端分别用 `gh` 当前登录态验证；安装后核对版本及 release manifest。

## Migration Plan

1. 新增失败回归测试，覆盖 `new` 配置继承和动作选择。
2. 修改 CLI 与动作脚本并完成完整测试。
3. 更新 README 的模式说明和配置示例。
4. 将版本提升到 `0.1.3`，提交并推送 `master` 与 `v0.1.3`。
5. 等待 GitHub release 三平台资产、校验文件和 commit 绑定全部完成。
6. 在 `sqbair` 重装插件，验证二进制为 `0.1.3` 且 Auto 新建能启动四个角色。

回滚时重新安装 `v0.1.2`；本变更不迁移数据库，已创建 Mission 仍可读取。

## Open Questions

无。
