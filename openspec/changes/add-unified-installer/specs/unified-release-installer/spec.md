## ADDED Requirements

### Requirement: 单一 Release 版本绑定
安装器 MUST 从自身发布资产或显式版本参数解析一个固定 tag，并以该 tag 安装 Herdr plugin、
CLI 与 Agent skill；不得在同一次安装中分别解析 latest。

#### Scenario: 固定版本无人值守安装
- **WHEN** 用户运行固定 tag 的 `install.sh` 并传入 `--yes --agents codex,claude`
- **THEN** plugin install 的 `--ref`、CLI 版本以及 canonical/Codex/Claude skill 内容均来自同一个 tag

### Requirement: 平台资产与完整性校验
安装器 MUST 只接受 Release 实际支持的 macOS/Linux 平台组合，并在写入任何安装目标或调用
plugin install 前，依据同一 Release 的 `SHA256SUMS` 校验 CLI 与 skill archive。

#### Scenario: 校验失败保留既有安装
- **WHEN** CLI 或 skill archive 的 SHA-256 与 Release 清单不一致
- **THEN** 安装器以非零状态退出，既有 CLI、canonical skill、Agent skill 路径和 plugin 均保持不变

#### Scenario: 不支持的平台失败
- **WHEN** 当前 OS/architecture 没有对应的 Release CLI 资产
- **THEN** 安装器在产生安装副作用前给出明确错误并退出

### Requirement: CLI 原子安装与验证
安装器 MUST 把校验后的 CLI 原子安装为 `~/.local/bin/herdr-mission`、设置可执行权限，并核对
其报告版本与固定 tag 一致；`~/.local/bin` 不在 `PATH` 时 MUST 给出提示但不得把成功变成失败。

#### Scenario: 已有 CLI 的安全替换
- **WHEN** 所有预检通过且目标位置已有旧 CLI
- **THEN** 旧文件只在新 CLI 已校验并准备完成后被原子替换

### Requirement: Agent skill 选择与 canonical 安装
安装器 MUST 支持 Codex、Claude Code 或两者，并把校验后的 skill 安装到
`~/.local/share/herdr-mission/skills/herdr-mission-team`；Codex 与 Claude Code 的用户级
skill 路径 MUST 分别为 `~/.agents/skills/herdr-mission-team` 与
`~/.claude/skills/herdr-mission-team`，且都必须是与 canonical 内容一致的独立普通目录副本。

#### Scenario: 管道调用中的交互选择
- **WHEN** stdin 用于传输脚本且用户没有提供 `--agents`
- **THEN** 安装器从 `/dev/tty` 读取 Codex、Claude Code 或两者的选择

#### Scenario: 两种 Agent 同时安装
- **WHEN** 用户选择 Codex 与 Claude Code
- **THEN** 两个用户级 skill 路径均为非符号链接的受管目录，且 `SKILL.md` 与 canonical 完全一致

### Requirement: Skill 冲突失败关闭与幂等
安装器 MUST 只更新带自身有效所有权标记的 canonical 与 Agent skill 普通目录；任何外来
文件、目录或 symlink MUST 原样保留并导致安装失败。每个已有受管副本 MUST 通过同目录
暂存文件原子替换 `SKILL.md`，并在预检后、实际写入前重新验证所有权。新副本 MUST 在同父
目录完整暂存后以目标平台的原子 no-replace rename 发布，不得先公开空目录再写入所有权标记。

#### Scenario: 重复安装相同版本
- **WHEN** 相同 tag 已完整安装且用户再次执行相同选择
- **THEN** 安装成功且 canonical 与所选 Agent 路径仍各只有一个内容一致的受管副本

#### Scenario: 外来 Agent skill 占位
- **WHEN** 任一所选 Agent skill 路径已被外来文件、目录或 symlink 占用，或 plugin 外部 hook
  返回后、开始本地提交前被替换
- **THEN** 安装器以非零状态退出并保持所有冲突路径与尚未开始提交的本地安装不变

#### Scenario: 多副本提交途中失败
- **WHEN** 某个目标的 `SKILL.md` 原子切换失败
- **THEN** 安装器以非零状态退出，每个已处理目标只呈现完整旧版或完整新版，并可用同一固定 tag
  重跑使 canonical 与全部所选 Agent 副本收敛一致

### Requirement: Herdr plugin 安装与最终证明
安装器 MUST 在预检通过后执行 `herdr plugin install gxbsst/herdr-mission --ref <tag> --yes`，
并在结束前核对 plugin resolved commit 与 Release `COMMIT`、CLI version 和 skill frontmatter。

#### Scenario: 缺少 Herdr
- **WHEN** `herdr` 不在 `PATH`
- **THEN** 安装器给出安装前置条件错误并在产生安装副作用前退出

#### Scenario: 三组件安装完成
- **WHEN** 所有下载、安装和最终核对均成功
- **THEN** 安装器以零状态退出并报告固定 tag、CLI 路径与所选 Agent skill

### Requirement: Release 资产完整
GitHub Release MUST 发布写入当前 tag 的 `install.sh`、skill archive、支持平台的 CLI、
`SHA256SUMS` 与 `COMMIT`，且 checksum MUST 覆盖 skill archive 与全部 CLI 资产。

#### Scenario: Tag 发布
- **WHEN** `v*` tag 触发 Release workflow
- **THEN** Release 包含统一安装所需的全部不可变资产
