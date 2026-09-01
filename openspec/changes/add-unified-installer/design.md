## Context

现有插件安装会把二进制构建在 Herdr 管理的 plugin checkout 内，并在 build hook 中安装
快捷键；它不保证 `herdr-mission` 在用户 `PATH` 上，也不安装 Agent skill。三个组件若由
用户分别获取，会出现 tag 漂移、未校验下载或覆盖已有 skill 的风险。

安装器需要兼容 macOS/Linux 的系统 `/bin/sh`，且典型调用中 stdin 已被 `curl | sh`
占用。仓库当前 Release 有 macOS arm64、macOS x86_64 与 Linux x86_64-musl 三个 CLI
资产，并生成 `SHA256SUMS` 和 `COMMIT`。

## Goals / Non-Goals

**Goals:**

- 以单一、固定的 release tag 安装并验证 plugin、CLI 与 skill。
- 同时支持交互安装和 CI/自动化的无人值守安装。
- 对已拥有的安装幂等升级，对来源不明的路径失败关闭。
- 安装中途失败时不暴露半写入的 CLI 或单个 Agent skill 副本。

**Non-Goals:**

- 不改变 `herdr plugin install` 的职责或 Herdr Core API。
- 不安装 Herdr 本身，不管理 shell profile，也不自动删除旧安装。
- 不为当前 Release workflow 尚未构建的平台承诺预编译 CLI。

## Decisions

### 1. Release 产物写入不可变 tag

仓库保留带 `@HERDR_MISSION_RELEASE_TAG@` 占位符的 `install.sh`。Release workflow 将
当前 `GITHUB_REF_NAME` 写入发布副本；因此 `latest/download/install.sh` 与
`releases/download/vX.Y.Z/install.sh` 得到的脚本都天然固定到自身所在 tag。测试可通过
环境变量提供 tag 与 release base URL，但生产默认不从 wall-clock 或多个“latest”请求推断版本。

备选方案是运行时查询 GitHub latest API；该方案在多个下载之间可能跨越新发布，且增加 JSON
解析依赖，因此拒绝。

### 2. Skill 作为 checksum 覆盖的 archive 发布

canonical `skills/herdr-mission-team` 被打成 `herdr-mission-team.skill.tar.gz`，与目标
平台 CLI 一起写入 `SHA256SUMS`。安装器先下载全部目标、严格匹配 checksum 条目并校验，
再开始安装。archive 只允许预期的 skill 文件布局，避免路径穿越或夹带文件。

备选方案是读取 GitHub raw branch；它不是不可变 release payload，无法和 CLI 共享同一
校验边界，因此拒绝。

### 3. CLI 与 skill 副本分阶段原子落盘

CLI 校验后写入 `~/.local/bin` 同目录临时文件，设为可执行，再用 rename 替换目标。
canonical skill 位于 `~/.local/share/herdr-mission/skills/herdr-mission-team`。canonical、
Codex 与 Claude Code 目标各自保存完整 `SKILL.md` 和稳定的安装器所有权标记。新目录以
同父目录 staging 副本写入 marker 与 `SKILL.md` 并 fsync，再由已校验 CLI 的内部 helper
持有父目录 FD，以 macOS `RENAME_EXCL` 或 Linux `RENAME_NOREPLACE` 原子发布；目标在预检后
出现 file、directory 或 symlink 时失败关闭且不覆盖。staging 名称使用系统随机源，不依赖
可由崩溃遗留耗尽的 PID/有限序号槽；相同固定 tag 重跑会使用新 staging 并继续收敛。已有受管目录和恢复路径都只通过
同目录临时文件原子替换 `SKILL.md`，不把 release 版本拆分到另一个需要同步替换的 marker。
安装前以及 plugin 外部 hook 返回后都重新验证 CLI 和所有 skill 目标，避免预检后替换。

### 4. Agent skill 使用受检直接副本

Codex 目标为 `~/.agents/skills/herdr-mission-team`，Claude Code 目标为
`~/.claude/skills/herdr-mission-team`。安装器把已校验的同一 skill payload 直接复制到
所选目录；只更新带精确所有权标记的普通目录，任何 symlink 或外来文件/目录均不覆盖。
安装结束时逐一比较 canonical 与所选 Agent 的 `SKILL.md`，证明三者来自同一固定 Release。
多个路径无法形成跨文件系统的单次原子事务，但每个公开 skill 副本都只以完整文件切换，
不会暴露部分正文。已有目录的写入先以 `cd -P` 锚定并在目录内重新验证 marker，再通过
相对路径提交；若公开路径随后被替换，安装器不会写入替代目录，并在最终验证时失败。

### 5. 交互输入与验证

交互选择从 `/dev/tty` 读取，避免消费管道 stdin。`--yes --agents codex,claude` 提供完整
无人值守路径。安装完成后通过 `herdr plugin list --plugin ... --json` 精确核对唯一目标记录的
source kind、owner、repo、requested ref 与 `resolved_commit`，并核对 CLI 返回的
`binary_version` 以及 skill frontmatter；缺少 `~/.local/bin` 的 `PATH` 只发出明确警告。

## Risks / Trade-offs

- [运行 `curl | sh` 依赖 TLS 与 GitHub 账户安全] -> 文档同时提供固定 tag URL，下载的 CLI
  与 skill 由 release checksum 二次验证，脚本自身由 GitHub Release 托管。
- [plugin install 先于本地文件安装，后续失败可能留下已更新 plugin] -> 所有下载、checksum、
  平台和 skill 目标冲突检查必须在调用 Herdr 前完成；Herdr 自身安装仍由其事务边界负责。
- [skill 副本更新后已有 Agent 进程未立即重载] -> 安装器只保证磁盘状态，文档提示重启
  相应 Agent 会话；下一次幂等安装会重新校验并同步所有所选副本。
- [不同 Unix 工具参数存在差异] -> 脚本只使用 POSIX shell 与 macOS/Linux 均有的工具，
  SHA-256 同时支持 `sha256sum` 和 `shasum -a 256`。
- [canonical、Codex 与 Claude 是三个独立路径，无法跨目录原子提交] -> 每个 `SKILL.md`
  独立原子切换，任何失败返回非零并可用相同固定 tag 重跑收敛；规范不承诺批量全有或全无。
- [非协作进程可在最终复验之后继续修改用户目录] -> 安装器精确复验自身调用的 plugin hook
  边界并锚定 skill 目录，但不把同一用户主动并发篡改安装路径纳入事务保证；安装时不得并发
  修改 CLI 或 skill 目标。

## Migration Plan

1. 合并 installer、skill、测试与 workflow 变更。
2. 下一次 `v*` Release 自动发布新增资产。
3. 用户可直接运行统一安装器；旧的仅插件命令继续有效。
4. 回滚时删除新 Release 资产生成步骤并恢复 README 首选入口，不需要迁移数据库或 Rust API。

## Open Questions

无。
