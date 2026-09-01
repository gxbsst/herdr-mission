## Why

当前 `herdr plugin install` 只负责插件生命周期，用户仍需分别处理可直接调用的
`herdr-mission` CLI 与 Agent skill，容易得到版本不一致或只有部分能力可用的安装。
需要一个可从 GitHub Release 直接执行的统一安装入口，一次完成三部分安装并验证结果。

## What Changes

- 新增可通过 `curl ... | sh` 执行的统一安装器，支持交互选择 Codex、Claude Code 或两者，
  并支持无人值守参数。
- 插件、CLI 与 skill 必须绑定同一个不可变 release tag；CLI 与 skill archive 下载后必须
  通过 release `SHA256SUMS` 校验。
- CLI 原子安装到 `~/.local/bin/herdr-mission`；skill 保存一份 canonical 副本，并把同一
  已校验内容直接复制到所选 Agent 的用户级 skill 目录，不依赖符号链接。
- 安装器幂等处理自身已安装的 skill 副本；遇到外来文件、目录或符号链接冲突时失败关闭，
  不静默覆盖。
- GitHub Release 增加已写入 tag 的 `install.sh` 与校验覆盖的 skill archive，并更新安装文档。

## Capabilities

### New Capabilities

- `unified-release-installer`: 定义固定版本的插件、CLI 与 Agent skill 统一安装、验证、幂等和冲突处理契约。

### Modified Capabilities

无。

## Impact

- 新增仓库根目录 `install.sh`、canonical `skills/herdr-mission-team/SKILL.md` 与安装器集成测试。
- `.github/workflows/release.yml` 新增 installer/skill Release 资产与 checksum 生成。
- `README.md` 的首选安装入口变为统一安装器；原始 `herdr plugin install` 仍可作为仅插件安装方式。
- 不新增 Rust 或 shell 运行时依赖，不改变 Herdr Core、Mission 数据库、公开 Rust API 或现有插件生命周期。
