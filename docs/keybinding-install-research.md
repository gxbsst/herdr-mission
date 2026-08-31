# Herdr v0.8.2 插件 keybinding 与安装生命周期核对

## 范围与版本

- 核对对象是 Herdr `v0.8.2`；官方 tag 指向提交 `9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c`。
- 本机 `/Users/weston/.local/bin/herdr --version` 输出 `herdr 0.8.2`。
- 证据只取官方仓库 `herdrdev/herdr` 的固定提交源码、测试和文档；未使用滚动分支内容。

## 结论

### 1. v0.8.2 plugin manifest 没有 keybinding、keys 或 prefix 字段

`herdr-plugin.toml` 的反序列化结构只声明包元数据，以及 `build`、`startup`、`actions`、`events`、`panes`、`link_handlers`；没有 `keybinding`、`keybindings`、`keys` 或 `prefix` 字段。按键只能由用户的 Herdr `config.toml` 配置。

一手证据：

- [`RawPluginManifest` 完整顶层字段](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/app/api/plugins/manifest.rs#L11-L34)
- [`InstalledPluginInfo` 暴露的 manifest 能力](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/api/schema/plugins.rs#L37-L67)
- [插件文档的完整 manifest 示例与入口列表](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/docs/next/website/src/content/docs/plugins.mdx#L54-L104)
- [插件 action 的用户 keybinding 示例](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/docs/next/website/src/content/docs/plugins.mdx#L331-L341)
- [Herdr 用户配置文件位置](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/docs/next/website/src/content/docs/configuration.mdx#L10-L17)

因此，不能在 plugin manifest 中原生声明默认快捷键。插件侧可行路径是安装命令在用户确认后修改 `config.toml`，并明确处理既有 prefix、按键冲突和失败回滚。

### 2. build 的顺序只适用于 install；link 不运行 build

GitHub `plugin install` 的精确顺序是：

1. 检出临时目录，解析 manifest，并检查能否替换已有插件。
2. 打印 preview；交互模式等待确认，`--yes` 跳过提问。
3. 按 manifest 顺序运行当前平台支持的 `[[build]]` 命令。
4. 复核 build 前后的 manifest 完全一致。
5. 移动到托管目录，重新读取最终 manifest。
6. 通过 `PluginLink` 注册；注册失败会回滚托管 checkout。

一手证据：

- [checkout、preview、确认、build 与 build 后复核](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/cli/plugin.rs#L188-L215)
- [移动托管目录后才注册及失败回滚](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/cli/plugin.rs#L217-L250)
- [build 按 manifest 顺序执行](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/cli/plugin.rs#L1266-L1284)
- [官方文档规定确认后、注册前执行](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/docs/next/website/src/content/docs/plugins.mdx#L218-L226)
- [失败 build 不注册且不留下托管 checkout](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/tests/cli/plugins.rs#L521-L612)

`plugin link` 没有 build 阶段：CLI 直接发送 `PluginLink`，server/offline 路径读取 manifest 后直接持久化。开发者本地 `link` 前必须自行构建。

- [link CLI 直接发送 `PluginLink`](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/cli/plugin.rs#L48-L85)
- [server link handler 不运行 build](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/app/api/plugins/mod.rs#L68-L88)
- [官方文档明确 `plugin link` 不运行 build](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/docs/next/website/src/content/docs/plugins.mdx#L220-L224)

### 3. startup 不在 install、link 或 enable 时执行

对每个 enabled 且 manifest 可用的插件，`[[startup]]` 在 Herdr 恢复 session、API socket 就绪后异步运行；live handoff 的接任 server 也会再运行一次。它不会在 install、link、enable 或 config reload 时运行。

- [官方 startup 生命周期说明](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/docs/next/website/src/content/docs/plugins.mdx#L233-L249)
- [startup runner 只选择 enabled 且可用的插件](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/app/api/plugins/runtime.rs#L183-L216)
- [正常 server ready 后调用 startup hooks](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/server/headless.rs#L5125-L5133)
- [live handoff 接任后再次调用](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/server/headless.rs#L5233-L5244)

因此不能依赖 startup 在安装完成后生成快捷键。Herdr v0.8.2 不提供更合适的插件生命周期入口，`plugin install` 的 build hook 是当前唯一能让 GitHub 安装自动落地配置的插件侧扩展点。

## 限制

- build 在最终插件注册前运行。若后续注册失败，Herdr 只回滚托管 checkout，不会自动回滚插件已经修改的用户配置。
- `plugin unlink` 没有 uninstall hook，无法自动移除快捷键。
- 本地 `plugin link` 不运行 build，不会自动安装快捷键。
- Herdr 0.8.2 没有配置 mutation API 或跨进程配置锁，Core 的设置写入也直接使用
  `fs::write`。插件安装器可以在替换前复核原内容并以同目录临时文件原子替换，但无法与
  不遵守同一锁协议的外部并发写入实现真正 compare-and-swap。
  [Core 配置写入实现](https://github.com/herdrdev/herdr/blob/9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c/src/app/config_io.rs#L6-L29)

这些限制意味着插件安装命令必须幂等、遇到按键冲突时零写入失败，并在 README 中明确卸载/本地 link 的边界。
