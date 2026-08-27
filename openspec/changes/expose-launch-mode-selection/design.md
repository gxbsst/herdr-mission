## Context

运行时的 `LaunchMode` 已经能够在 `auto` 时一次启动 PM、Worker、Scout、Reviewer，在 `manual` 时仅启动 PM。前两版修复了 CLI、action 和 fresh busy pane，但控制中心仍没有单次选择，模式也只存在于一次调用的 `LaunchOptions`。因此显式 Auto 创建后，角色 prompt 仍写 Manual，后续 `resume` 又可能从当前全局配置退回 Manual；Skill 没有可执行的切换入口。

本变更横跨 Rust CLI、交互式 shell 动作、测试和发布元数据，但不涉及 SQLite、Agent 启动协议或 Herdr Core。

## Goals / Non-Goals

**Goals:**

- 让 CLI `new`、TUI、`resume`、`start-role` 和 Skill 对 Mission 持久化的 `launch_mode` 使用一致语义。
- 让插件“新建 Team Mission”动作可以为当前任务选择 `auto` 或 `manual`。
- 让控制中心新建表单直接选择 Auto/Manual，并显示当前 Mission 模式。
- 让已有 Mission 可通过窄 CLI 命令切换模式，且 `status`/`init` 返回当前值。
- 明确优先级：单次显式参数高于全局配置，全局配置缺失或无效时为 `manual`。
- 保持错误输入失败关闭，不能在用户输入错误模式时静默创建 Mission。

**Non-Goals:**

- 不把全局默认从 `manual` 改为 `auto`。
- 不改变 Auto/Manual 在运行时启动哪些角色。
- 不增加 Herdr manifest 参数系统或依赖新的交互工具。
- 切换模式时不隐式停止或启动已有角色；需要补启动时显式运行 `resume`。
- 不修改冻结的 v3 coordination schema 版本。

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

### 4. fresh pane 的 busy 只在结构化归属验证后有限重试

`sqbair` 的 `v0.1.3` Auto canary 证明：连续启动角色时，刚由 runtime split 的 pane 可能已经存在且 cwd 正确，但 shell 尚未进入 Herdr 认为可用的状态；此时第一次 `agent start` 返回 `agent_pane_busy`，`pane get` 仍显示无 Agent session。旧逻辑只尝试接管已有 Agent，因此立即把 Mission 标记为 blocked。

runtime 将“当前调用刚分配的 pane”和“此前已持久化的 pane”区分处理。fresh pane 返回 busy 时，先结构化验证 pane ID、workspace、cwd、工作区 tab ID 与名称；只有全部匹配且没有 Agent/session，才按现有 250ms 间隔有限重试同一条 `agent start`。若 Agent 身份已经出现，则继续原有接管路径，不重复启动；若归属不匹配，立即失败关闭。

此前已持久化的空 pane 不启用重复 `agent start`，保留 `v0.1.2` 的保守边界，避免对可能正在运行非 Agent 前台进程的历史 pane 反复注入命令。没有选择固定 sleep，因为 pane 的实际 ready 时间不稳定，固定等待会无条件增加每个角色的启动延迟。

### 5. 以 v0.1.4 发布并复用现有预编译链路

`v0.1.3` 已完成入口发布并在 `sqbair` 发现上述 fresh pane 竞态。最终版本同步提升到 `0.1.4`；推送 tag 后仍由现有 GitHub Actions 构建三个目标平台，生成 `SHA256SUMS` 与 `COMMIT`。`sqbair` 继续通过已认证的 `herdr plugin install gxbsst/herdr-mission --yes` 下载预编译产物，不依赖 Python 或现场 Cargo 编译。

### 6. Mission 模式存放在 plugin-owned `mission_state`

`mission_state` 增加 `launch_mode TEXT NOT NULL DEFAULT 'manual'`。它属于 Rust plugin 自己的生命周期表，不修改冻结的 v3 coordination schema；bootstrap 通过 `PRAGMA table_info` 幂等补列，使旧库安全迁移为 Manual。

`CreateMissionRequest` 携带已经解析完成的模式，并与 Mission、角色在同一事务写入。`read_mission_status`、控制中心 overview 和角色 `init` 返回该值。`resume`、`start-role` 与角色 prompt 不再重新猜测全局配置，而是读取 Mission 状态。

没有把模式只写入 prompt 文件，因为 prompt 是启动时快照，无法作为后续切换的真源；也没有修改 `team_missions`，避免扩大冻结 coordination 表契约。

### 7. 控制中心选择与显式切换使用同一枚举

Team 新建表单在“布局”之后增加“启动模式”，默认值只在打开控制中心时从 `LaunchConfig` 读取一次。用户选择随 `Job::New` 传入 worker thread，后台任务不得再次读取配置覆盖单次选择。Simple 布局固定按单 Worker 启动，不显示 Team 模式选项。

CLI 增加 `set-launch-mode --mission-id <id> --launch-mode auto|manual`。该命令只原子更新策略并返回当前模式，不隐式创建 pane；Skill 若需要立即补齐 Auto 角色，应随后显式运行 `resume`。这种分离让“改策略”和“产生终端副作用”保持可审计。

### 8. fresh pane 短暂不可见时只等待当前分配目标

`v0.1.4` canary 显示 split 已返回 Worker pane ID，但 busy 恢复路径第一次 `pane get` 得到 `pane_not_found`。对数据库已记录或本次启动刚分配的精确 pane ID，这可能是 API 可见性延迟；runtime 在既有 250ms/20 次边界内只读重试 `pane get`，一旦可见仍必须完整校验 pane、workspace、cwd 与“工作区”tab。

历史 pane 可以使用同一只读可见性等待，但空 pane 后仍只允许一次 `agent start`；只有本次 fresh pane 才能在 `agent_pane_busy` 后重复同一条 `agent start`。任何非 `pane_not_found` 错误都不重试。超过上限仍保持 blocked，不创建替代 pane，避免隐藏真实丢失。

### 9. 以新版本发布并只做本地与资产验收

`v0.1.4` 保持不可变。新修复提升到后续版本，由 GitHub Actions 生成三平台预编译资产、`SHA256SUMS` 与 `COMMIT`。按用户要求不再在 `sqbair` 运行安装或 canary；发布验收以本机完整测试、release build、OpenSpec strict 校验和 GitHub release 字节校验为准。

## Risks / Trade-offs

- [交互动作多一次输入] -> 回车直接继承配置，保留最短操作路径。
- [用户误拼模式] -> 在调用二进制前失败关闭并列出仅接受的值。
- [无效配置被静默回退] -> 延续 `LaunchConfig::load` 现有兼容语义；`doctor` 行为不在本次扩大。
- [显式参数与配置读取顺序混淆] -> 用单一解析优先级函数和测试固定“显式覆盖 > 配置 > Manual”。
- [busy pane 实际已被移动或改作他用] -> 每次重试前核对 pane ID、workspace、cwd 和精确“工作区”tab；只允许本次 fresh pane 重试。
- [Agent 已启动但 session 延迟出现] -> 发现 provider 身份后只轮询接管，不再执行第二次 `agent start`。
- [模式切换后已运行 Agent 仍持有旧 prompt 快照] -> 每次 `init` 返回持久化当前模式并声明其为真源；切换命令不声称重写运行中的系统 prompt。
- [旧库没有模式列] -> bootstrap 在写入前幂等补列，所有旧 Mission 默认 Manual。
- [pane ID 实际永久消失] -> 只有限等待，不创建替代 pane；超时继续 blocked。
- [私有仓库 release 下载依赖认证] -> 发布端和安装端分别用 `gh` 当前登录态验证；安装后核对版本及 release manifest。

## Migration Plan

1. 新增失败回归测试，覆盖 `new` 配置继承和动作选择。
2. 修改 CLI 与动作脚本并完成完整测试。
3. 更新 README 的模式说明和配置示例。
4. 记录 `v0.1.3` canary 的 `agent_pane_busy` 失败，增加 fresh pane RED 测试并实现有限重试。
5. 将最终版本提升到 `0.1.4`，提交并推送 `master` 与 `v0.1.4`。
6. 等待 GitHub release 三平台资产、校验文件和 commit 绑定全部完成。
7. 记录 `v0.1.4` canary 的 `pane_not_found` 失败，停止远端 canary 验证。
8. 增加模式持久化、TUI 选择、CLI/Skill 切换与 fresh pane 延迟可见的回归测试。
9. 实现并完成本机完整验证，发布新 tag，校验三平台预编译资产。

回滚二进制时保留新增列；旧版本会忽略 plugin-owned `launch_mode`，不会破坏已有 Mission 数据。

## Open Questions

无。
