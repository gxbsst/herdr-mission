---
name: herdr-mission-team
description: 通过 PM、Worker、Scout 和 Reviewer 协调 Herdr Kit 团队 Mission（Rust 版）。当 HERDR_MISSION_ID 和 HERDR_MISSION_ROLE 已设置，或用户要求按 Mission/Assignment ID 续接工作时自动使用。
---

# Herdr Mission 团队

这是 Herdr Kit 团队 Mission 面向 Agent 的 Rust 协议。持久化事实来源是
`herdr-mission`（Rust 二进制）写进 SQLite 的状态，不是终端文本。

## 协调二进制与数据库

二进制和数据库路径按下面顺序解析，找到就用：

1. 角色由 Runtime 启动时，初始 prompt 里已经注入 `{{bin}}`、`{{database}}`、
   `{{mission_id}}`、`{{role}}` 四个值，直接照用，不要臆测或改写。
2. 手动加入（join）场景没有注入，用确定路径：
   - 二进制：`$HERDR_PLUGIN_ROOT/target/release/herdr-mission`；若 `HERDR_PLUGIN_ROOT`
     未设置，用 `~/Projects/herdr-mission/target/release/herdr-mission`。
   - 数据库：`$HERDR_PLUGIN_STATE_DIR/missions.sqlite3`；若未设置，用
     `~/.local/state/herdr/plugins/weston.herdr-mission/missions.sqlite3`。

所有协调命令都写 `--database=<database>`，不要省略。`send`/`reply` 写队列后会自动
投递一次，不需要再手动 `deliver`；只在目标迟迟没反应时才可补一次
`herdr-mission deliver --json --database=<database>`。

## 每次会话开始

读自己的待办与收件箱（只读，不写库、不唤醒其它角色）：

```bash
herdr-mission init --json --mission-id=<mission_id> --role=<role> --database=<database>
```

`pending_assignments` 是派给你的 Assignment（用它的 `id` 作为 `--assignment`），
`inbox` 是收到的消息。有就处理，没有就简短待命。不要反复 `init`/`status` 探测，
不要尝试 socket、Terminal、Computer Use 等权限绕行。

## PM

PM 负责协调，不得编辑 Mission 工作树。

`launch_mode` 默认 `manual`：没有明确用户指令、PM inbox 消息或已有 Assignment 时，
只报告“PM 已就绪”并等待，不自动拉起其它角色。`auto` 模式下，明确且边界清晰的
简报就是派单依据，选择最小必要角色继续流转，但占位标题、实质分支、外部写入、
新依赖、push、merge、deploy 或破坏性操作仍必须停下报告。

收到明确任务后，先 `start-role` 拉起目标角色（幂等，已启动会自动跳过），再 `send`：

```bash
herdr-mission start-role --json --mission-id=<mission_id> --role=scout --database=<database>
herdr-mission start-role --json --mission-id=<mission_id> --role=worker --database=<database>
herdr-mission start-role --json --mission-id=<mission_id> --role=reviewer --database=<database>
```

```bash
herdr-mission send --json --mission-id=<mission_id> --role=pm --target=scout --kind=task --body='<只读调查任务>' --database=<database>
herdr-mission send --json --mission-id=<mission_id> --role=pm --target=worker --kind=task --body='<写代码任务>' --database=<database>
herdr-mission send --json --mission-id=<mission_id> --role=pm --target=reviewer --kind=review --body='<要审核的产出>' --database=<database>
```

派单规则：范围/文件/验收已明确时直接 `send worker`；只有缺仓库事实、任务边界或
风险证据时才 `send scout` 做只读调查。收到各 Scout finding 后先审核证据、范围和
风险，再决定是否派 Worker。Worker 完成后派 Reviewer 审核。可复用决策写进
`--body` 的说明里，不要只记在屏幕输出上。

## Scout

Scout 保持只读，只调查分配给本角色的范围，引用具体文件和符号，然后回执：

```bash
herdr-mission reply --json --mission-id=<mission_id> --role=scout --assignment=<assignment_id> --kind=finding --body='<调查结论 + 证据 + 来源>' --database=<database>
```

拿不到证据时：

```bash
herdr-mission reply --json --mission-id=<mission_id> --role=scout --assignment=<assignment_id> --kind=blocked --body='<阻塞原因>' --database=<database>
```

不得编辑文件或声称已完成实施。

## Worker

Worker 是唯一允许修改 Mission 工作树的角色。实施、测试并报告具体文件和验证证据：

```bash
herdr-mission reply --json --mission-id=<mission_id> --role=worker --assignment=<assignment_id> --kind=completed --body='<改动文件 + 验证证据 + 结果>' --database=<database>
```

无法继续时：

```bash
herdr-mission reply --json --mission-id=<mission_id> --role=worker --assignment=<assignment_id> --kind=blocked --body='<阻塞 + 需要的决策>' --database=<database>
```

只做本 Assignment 范围内的修改；不越界改共享文件、不自行扩大范围、不发布。

## Reviewer

Reviewer 保持只读，核对真实文件与差异后给出判定：

```bash
herdr-mission reply --json --mission-id=<mission_id> --role=reviewer --assignment=<assignment_id> --kind=approved --body='<通过理由>' --database=<database>
```

不通过时：

```bash
herdr-mission reply --json --mission-id=<mission_id> --role=reviewer --assignment=<assignment_id> --kind=rejected --body='<阻断项 + 最小修复要求>' --database=<database>
```

只在硬条件全部满足时 approved。

## 手动加入（join）

当用户把“当前 agent”作为某 Mission 的 PM、worker、scout 或 reviewer 加入时
（例如关闭旧 pane 后重新挂接 PM，或“把当前 agent 当作 mission X 的 worker”），
由你执行下面步骤并汇报结果。`--mission` 接受 Mission ID（`msn-…`）或唯一标题；
标题匹配到多个时改用 ID。

```bash
herdr-mission join --mission <mission-id|标题> --role <pm|worker|scout|reviewer> \
  [--pane <pane-id>] [--agent-name <name>] --database=<database>
```

- `--pane` 缺省读当前 pane（`HERDR_PANE_ID`），读不到就要求显式传。
- 同一角色重复 join 会用当前 pane 替换旧绑定，适合 pane 被关掉后重新挂上。
- join 成功会返回 `mission_id`/`role`/`pane_id`，之后用上面的 `init` 读待办。

## 规则

- 不要把屏幕输出或 Agent 声明当作持久化团队状态；SQLite 才是真源。
- 始终使用已投递 Prompt 或 `init` 返回的 Assignment ID，不要凭记忆猜。
- PM 与所有角色通信；非 PM 角色只能回执 PM。
- 外部副作用（push、merge、deploy、发布）前必须先向用户说明并等待确认。
