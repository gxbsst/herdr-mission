# Herdr Mission

独立 Rust 版 Team Mission runtime，通过 PM、Worker、Scout、Reviewer 四个角色协调
团队 Mission。持久化事实来源是插件自己的 SQLite，不依赖终端文本或 Python。

## 安装

推荐使用 GitHub Release 的统一安装器，一次安装 Herdr plugin、可直接从 shell 调用的
`herdr-mission` CLI，以及 Codex/Claude Code 的 `$herdr-mission-team` skill：

```sh
curl -fsSL https://github.com/gxbsst/herdr-mission/releases/latest/download/install.sh \
  | sh
```

脚本从 `/dev/tty` 询问 skill 要安装给 Codex、Claude Code 或两者；stdin 可以继续安全地
承载 `curl | sh`。无人值守安装两者：

```sh
curl -fsSL https://github.com/gxbsst/herdr-mission/releases/latest/download/install.sh \
  | sh -s -- --yes --agents codex,claude
```

需要锁定版本时，使用同一个 tag 下的安装器；该脚本会把 plugin、CLI 与 skill 全部固定
到这个 Release，不会再次解析 `latest`：

```sh
curl -fsSL https://github.com/gxbsst/herdr-mission/releases/download/vX.Y.Z/install.sh \
  | sh
```

安装位置：

- CLI：`~/.local/bin/herdr-mission`
- canonical skill：`~/.local/share/herdr-mission/skills/herdr-mission-team`
- Codex：`~/.agents/skills/herdr-mission-team`（独立受管副本）
- Claude Code：`~/.claude/skills/herdr-mission-team`（独立受管副本）

安装器不会修改 shell profile；如果 `~/.local/bin` 不在 `PATH`，会给出添加提示。遇到
外来的同名 skill 文件、目录或 symlink 时会保留原内容并终止，不会静默覆盖。重复安装或
升级只会更新带安装器所有权标记的普通目录，并重新核对所选副本与 canonical 内容一致。

如果只需要 Herdr plugin，不需要全局 CLI 或 Agent skill，仍可使用窄安装路径：

```sh
herdr plugin install gxbsst/herdr-mission --yes
```

两种入口最终都会让 Herdr clone 本仓库并执行 `fetch-or-build.sh`：优先从 GitHub Releases 下载
匹配版本+平台的预编译二进制并校验 SHA-256，下载失败才回退到源码编译。随后安装器会
自动写入 Mission 看板快捷键：

- 尚无 Herdr `config.toml` 时创建 `prefix = "ctrl+a"`，按 `ctrl+a` 后再按 `m` 打开看板。
- 已有显式 prefix 时保持不变，仍使用该 prefix 后再按 `m`。
- 已有配置但未显式声明 prefix 时保持 Herdr 默认 `ctrl+b`，不会改动其他快捷键的 prefix。
- 已知的旧 Mission 绑定（Herdr Kit Mission center 或早期 `mission-new`）会原子迁移到当前看板命令。
- `prefix+m` 已被其他命令占用时安装失败，原配置保持不变。

这是 Herdr 0.8.2 的插件生命周期限制下采用的安装期配置：plugin manifest 不能原生声明
快捷键，GitHub `plugin install` 会运行 build hook，而本地 `plugin link` 不会。`plugin
unlink` 也没有卸载 hook，因此卸载后如需移除快捷键，应手动删除对应的
`[[keys.command]]`。官方能力核对见
[`docs/keybinding-install-research.md`](docs/keybinding-install-research.md)。
安装期间请避免同时在另一个进程保存 Herdr 设置；Herdr 0.8.2 尚未提供配置写入 API
或跨进程配置锁。

## 前置依赖

- [herdr](https://herdr.dev) ≥ 0.8.0
- 有预编译二进制时无需 Rust；仅当回退源码编译时才需要 Rust 1.88.0
- [mise](https://mise.jdx.dev) + Rust 1.88.0（`mise use rust@1.88.0`），仅在本地构建/回退编译时需要

每个平台安装的是单一可执行文件，运行时无 Rust 或 Python 依赖。

## 从源码构建

```sh
mise exec rust@1.88.0 -- cargo build --release --locked
mise exec rust@1.88.0 -- cargo test --locked
```

产物在 `target/release/herdr-mission`。

## 发布

打 `v*` tag 并 push 会触发 `.github/workflows/release.yml`，编译三个平台
（macOS arm64 / macOS x86_64 / Linux x86_64-musl）并上传到 GitHub Release，
同时发布已写入当前 tag 的 `install.sh`、`herdr-mission-team.skill.tar.gz`，并生成
`SHA256SUMS` 和 `COMMIT`。CLI 与 skill archive 都由 checksum 覆盖，供统一安装器与
`fetch-or-build.sh` 校验。

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

## 命令

```text
new         创建 Mission
list        列出所有 Mission
status      查看单个 Mission 状态
set-launch-mode  切换 Mission 的 Auto/Manual 模式
init        读取角色待办与收件箱
send        派发 Assignment 给目标角色
reply       回执 Assignment
deliver     投递 outbox
reconcile   协调 Agent 实时状态并投递 outbox
start-role  按需启动一个角色
join        手动把当前 agent 加入为某角色
resume      恢复未启动的角色
delete      删除 Mission
tui         打开控制台
doctor      自检
daemon      常驻投递 daemon
stop        停止 daemon
manifest    校验二进制
```

用 `herdr-mission <command> --help` 查看单个命令用法。

## Mission 启动模式

Team Mission 支持两种角色启动模式：

- `manual`：只立即启动 PM，Worker、Scout、Reviewer 由 PM 按需启动；这是内置默认值。
- `auto`：创建 Mission 后立即启动 PM、Worker、Scout、Reviewer。

从 Herdr 执行“新建 Team Mission”时，可以为当前任务输入 `auto` 或 `manual`；
直接回车则使用全局配置。直接调用 CLI 时也可以显式覆盖：

```sh
herdr-mission new --title "任务名" --launch-mode auto
```

控制中心的 Team Mission 新建表单也有 Auto/Manual 选择。最终模式会写入 Mission
状态；之后 `status`、`init`、`resume`、`start-role` 和新生成的角色 prompt 都以这个
持久化值为准，不会因全局配置变化而改变。

全局默认值配置在 `~/.config/herdr-mission/config.toml`：

```toml
[launch]
launch_mode = "auto"
```

优先级为：当前命令的 `--launch-mode` > 全局配置 > 内置 `manual`。配置文件
缺失、无法读取或内容无效时会安全回退为 `manual`。

已有 Mission 可以显式切换策略：

```sh
herdr-mission set-launch-mode \
  --mission-id <mission-id> \
  --launch-mode auto
```

该命令只更新模式，不会隐式创建或关闭 pane。切换为 Auto 后如需立即补启动尚未运行
的角色，再显式执行：

```sh
herdr-mission resume --mission-id <mission-id>
```

## 结构

```text
src/           Rust 源码（kernel、store、runtime、TUI、CLI）
tests/         集成测试与 schema fixture
prompts/roles/ 每个角色的启动 prompt
actions/       herdr action 脚本
panes/         herdr pane（Mission 看板）
```

状态数据库默认在 `$HERDR_PLUGIN_STATE_DIR/missions.sqlite3`，由 herdr 注入；脱离
herdr 运行时回退到 `~/.local/state/herdr/plugins/weston.herdr-mission/missions.sqlite3`。
