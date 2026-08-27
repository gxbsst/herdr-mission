## Context

Mission 启动角色时会先为角色准备 worktree 和 pane，再调用 `herdr agent start`。Herdr Core 可能已经在 pane 中启动 Codex，但 Agent 名称或 session 的确认晚于调用方的等待窗口；这时命令返回超时，数据库中的 `team_roles.pane_id` 仍为空。后续重试把同一 pane 视为忙碌并再次失败，最终把实际已有可用 Agent 的 Mission 标记为 `blocked`。

Mission 同时维护执行、审查、验证三个区域。现有默认执行区域名为 `Mission 工作区`，且复用 workspace 初始 tab 时没有显式重命名，无法保证布局与界面约定一致。

本变更跨越 runtime 控制流、Herdr 命令适配、配置默认值和集成测试，但不改变数据库 schema、插件 ID 或公开 CLI。

## Goals / Non-Goals

**Goals:**

- 在 `agent start` 超时或报告 `agent_pane_busy` 后，可靠识别并接管已经在目标 pane 中运行的预期 Agent。
- 接管前同时验证 pane ID、Agent provider、Mission worktree cwd 和区域 tab，避免把无关进程错误归属到 Mission。
- 将三个区域的精确名称固定为 `工作区`、`审查`、`验证`，并确保所有 Agent 仅在 `工作区` 启动或恢复。
- 保持启动和恢复幂等，不重复创建 Agent 或区域 tab。

**Non-Goals:**

- 不延长 Herdr Core 的通用 Agent 启动超时。
- 不从终端文本、提示符或进程标题猜测 Agent 身份。
- 不自动接管 provider、cwd 或 tab 不匹配的 pane。
- 不修改 Mission SQLite schema 或迁移现有 Mission 数据。

## Decisions

### 1. 在 plugin runtime 内执行迟到成功恢复

`start_agent` 仍以 `herdr agent start` 为主路径。仅当错误属于超时或 `agent_pane_busy` 时，runtime 才调用结构化 `herdr pane get <pane_id>`，验证当前 pane 是否已经包含预期 Agent。验证通过后调用 `herdr agent rename <pane_id> <expected-name>`，并让原有持久化流程记录 pane。

超时表示 Agent 可能仍在异步完成启动，因此只轮询结构化状态，不重复执行 `agent start`。provider、cwd 和区域已经匹配但 Agent session 尚未上报时仍视为 pending，继续轮询到 session 出现或预算耗尽。`agent_pane_busy` 只进行一次接管判断：Herdr 0.8.2 的 pane 状态没有前台进程类型字段，无法安全区分空闲 shell 与无关非 Agent 进程；若不能证明目标 Agent 已就绪，当前调用立即失败关闭。后续显式恢复可以在同一已持久化 pane 上重新尝试，不会新建 pane。

选择 plugin runtime 而不是单纯增加固定等待时间，是因为已观察到 Core 在短时间内完成进程启动但名称查询超时；增加等待只能降低概率，不能处理后续的 pane busy 重试。也不修改 Herdr Core，因为 Mission runtime 掌握预期 provider、worktree 和区域语义，能够完成更严格的归属判断。

### 2. 只信任结构化 pane 状态

恢复判断先从 `herdr pane get` JSON 读取 pane ID、Agent provider、foreground cwd（兼容结构化结果中的 cwd 字段）、Agent session 和 tab ID，再从 `herdr tab get` JSON 读取 tab 名称。必须同时满足：

- 返回的 pane ID 与目标 pane 完全一致；
- provider 与角色配置的 Agent kind 完全一致；
- cwd 规范化后与 Mission worktree 完全一致；
- pane 所在 tab 的名称为 `工作区`。

Agent session 用于确认 pane 中确有 Agent 会话，但不替代以上归属条件。任何字段缺失、命令失败、JSON 无效或条件不匹配都返回原始启动失败并保持 fail-closed。

选择完整四项核验而不是仅比较 provider，是为了避免同一 provider 的其他任务被错误接管；选择结构化 JSON 而不是终端内容，是为了避免提示符、历史输出和本地化文本造成误判。

### 3. 用 rename 建立稳定 Agent 名称

split 或选择 root pane 后，先把 `pane_id` 与空的稳定名称写入现有角色记录，表达“已分配、未完成启动”。验证通过后使用现有 `herdr agent rename` 命令，将目标 pane 注册为 Mission 预期的稳定角色名称，再持久化完整运行时身份。显式恢复遇到已分配但稳定名称为空的角色时，先检查并接管该 pane 中已经运行的 Agent；只有结构化状态确认尚无 Agent 时才重新执行 `agent start`，从而避开稳定名称已经注册时的 `agent_name_taken`。`herdr agent get <stable-name>` 和 Mission 恢复最终都指向同一 Agent。

不新增数据库字段或“adopted”标志，因为现有 `pane_id` 与 legacy `terminal_id` 的空/非空组合足以表达分配中和启动完成两种状态。

### 4. 将区域名称和存在性作为精确运行时契约

三个区域固定为 `工作区`、`审查`、`验证`，不再允许配置文件改名或关闭。创建或恢复 workspace 时，复用初始 tab 作为 Agent 区域，并显式执行 tab rename 将其命名为 `工作区`；另外两个区域按固定名称创建或复用。区域 ID 缺失时，先用结构化 `tab list --workspace` 查找唯一同名固定区域：唯一命中就复用，多个命中失败关闭，没有命中才创建。每成功重命名、发现或创建一个区域就立即持久化对应 tab ID，使 create 成功但 SQLite upsert 失败后的重试也不会重复区域。

当 `工作区` 的 tab ID 和 root pane ID 都未持久化时，唯一同名 tab 只证明区域身份，仍不足以选择 Agent 锚点。系统继续使用结构化 `pane list --workspace` 筛选该 tab；只有恰好一个 pane 时才把它恢复为 root pane，零个或多个 pane 都失败关闭，避免把 Agent split 到不确定的 pane。

旧配置中的 `tab_mode = "tabs"` 仍可解析以保持配置文件兼容，但运行时将其解释为固定三区域布局，不再为每个角色创建独立 tab。旧的区域名称和开关字段也可被 TOML 解析器忽略，但不能改变固定布局。该布局同时覆盖 Simple 和 Team Mission，使 Team 的 PM 及后续按需角色都位于同一 `工作区`。

不保留 `Mission 工作区` 别名，因为本次要求的是界面和恢复判断使用唯一精确名称。Mission 重新启动或恢复布局时会结构化读取三个已持久化 tab，并分别收敛为 `工作区`、`审查`、`验证`。对于旧 `tab_mode = "tabs"` Mission，已完成角色如果仍在独立 tab，使用 Herdr `pane move` 搬入 `工作区`；移动最后一个 pane 时 Herdr 自动关闭空的旧 tab，使界面真正只保留三个区域。

审查和验证工具命令是区域创建后的 best-effort 初始化。若工具启动失败，保留已持久化且可用的区域 shell，并输出警告，不把区域变成无法判断是否需要重放工具命令的半成功状态。

### 5. 按需启动先验证工作区域锚点

`start-role` 在创建新 pane 前读取 anchor pane 及其 tab 的结构化状态，并验证 pane ID、workspace、Mission worktree cwd、execution tab ID 和 `工作区` 名称。只有验证全部通过后才允许 split。这样 PM 不能误用 `审查`、`验证` 或其他 workspace 中的 pane 启动角色，且 split、prompt 和启动恢复始终使用持久化的 Mission worktree。

## Risks / Trade-offs

- [Herdr `pane get` JSON 字段随版本变化] -> parser 只接受已知结构化字段并对缺失字段失败关闭；用 fixture 测试兼容字段。
- [cwd 的符号链接或表现形式不同导致拒绝] -> 对双方做文件系统规范化后比较；规范化失败时保留原值比较，不放宽到前缀匹配。
- [旧 workspace 仍有 `Mission 工作区` tab] -> 布局确保步骤重命名复用 tab；不自动删除其他同名或历史 tab，避免破坏用户区域。
- [区域创建中途失败导致重试重复 tab] -> 每个成功步骤后立即持久化区域 ID；重试只补齐缺失区域。
- [tab create 成功但区域 ID 未落盘] -> 创建前结构化发现唯一固定名 tab；唯一命中复用，歧义失败关闭。
- [busy pane 无法区分 shell 与无关进程] -> 不在同次调用重复启动；失败关闭后由显式恢复复用已持久化 pane。
- [split 后 Agent 身份落盘失败导致重复 pane] -> 启动前先持久化 pane 分配，完整身份失败时恢复同一 pane。
- [稳定名称已注册但完整身份未落盘] -> 未完成 pane 在启动前先尝试安全接管，避免 `agent_name_taken` 阻断恢复。
- [旧 tabs 模式角色留在独立 tab] -> 恢复时验证角色身份并 `pane move` 到工作区域；空源 tab 由 Herdr 关闭。
- [区域工具命令启动失败] -> 保留固定区域 shell 并告警；不重复创建区域，也不把工具副作用当作 Agent 启动前提。
- [接管后 rename 失败] -> 不持久化成功，Mission 继续 blocked，避免产生无法通过稳定名称访问的半成功状态。
- [额外一次结构化查询增加失败路径延迟] -> 仅在 timeout 或 pane busy 时触发，正常启动路径无额外调用。

## Migration Plan

1. 先增加失败复现和三区域命名测试。
2. 实现结构化 pane 状态解析、接管判断和初始 tab rename。
3. 完成格式化、Clippy、测试和 release build。
4. 将插件版本提升为 `0.1.2`，提交并推送 `master` 和不可变 tag `v0.1.2`。
5. 等待现有 GitHub Actions 生成三平台二进制、`SHA256SUMS` 和 `COMMIT`。
6. 在 `sqbair` 通过 `herdr plugin install gxbsst/herdr-mission --yes` 安装预编译 release，并恢复现有 `rust-version` Mission。

回滚时重新安装 `v0.1.1` 可以恢复旧 runtime；数据库 schema 未变化，无需数据回滚。已由 `v0.1.2` 接管并持久化的 pane 仍是合法的旧版本运行状态。

## Open Questions

无。实现以当前 Herdr `pane get --json` 的实际结构和现有测试 runner 契约为准；若字段名与设计描述不同，在保持四项核验语义不变的前提下适配实际字段。
