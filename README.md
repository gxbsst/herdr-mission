# Herdr Mission

独立 Rust 版 Team Mission runtime，通过 PM、Worker、Scout、Reviewer 四个角色协调
团队 Mission。持久化事实来源是插件自己的 SQLite，不依赖终端文本或 Python。

## 安装

作为 herdr 插件从 GitHub 安装：

```sh
herdr plugin install gxbsst/herdr-mission --ref v0.1.0
```

安装时 herdr 会 clone 本仓库，执行 `fetch-or-build.sh`：优先从 GitHub Releases 下载
匹配版本+平台的预编译二进制并校验 SHA-256，下载失败才回退到源码编译。

## 前置依赖

- [herdr](https://herdr.dev) ≥ 0.7.5
- 有预编译二进制时无需 Rust；仅当回退源码编译时才需要 Rust 1.88.0
- [mise](https://mise.jdx.dev) + Rust 1.88.0（`mise use rust@1.88.0`），仅在本地构建/回退编译时需要

二进制本身是单一 `arm64` 可执行文件，运行时无外部依赖。

## 从源码构建

```sh
mise exec rust@1.88.0 -- cargo build --release --locked
mise exec rust@1.88.0 -- cargo test --locked
```

产物在 `target/release/herdr-mission`。

## 发布

打 `v*` tag 并 push 会触发 `.github/workflows/release.yml`，编译三个平台
（macOS arm64 / macOS x86_64 / Linux x86_64-musl）并上传到 GitHub Release，
同时生成 `SHA256SUMS` 和 `COMMIT` 供 `fetch-or-build.sh` 校验。

```sh
git tag v0.1.0
git push origin v0.1.0
```

## 命令

```text
new         创建 Mission
list        列出所有 Mission
status      查看单个 Mission 状态
init        读取角色待办与收件箱
send        派发 Assignment 给目标角色
reply       回执 Assignment
deliver     投递 outbox
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

## 结构

```text
src/           Rust 源码（kernel、store、runtime、TUI、CLI）
tests/         集成测试与 schema fixture
prompts/roles/ 每个角色的启动 prompt
actions/       herdr action 脚本
events/        herdr 事件脚本（投递 reconcile）
panes/         herdr pane（Mission 看板）
```

状态数据库默认在 `$HERDR_PLUGIN_STATE_DIR/missions.sqlite3`，由 herdr 注入；脱离
herdr 运行时回退到 `~/.local/share/herdr-mission/missions.sqlite3`。
