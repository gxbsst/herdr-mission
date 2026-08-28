## 1. 回归测试

- [x] 1.1 为结构化 `herdr agent list` parser 增加完整 snapshot、可选名称、未知状态和损坏 JSON 测试。
- [x] 1.2 为角色 health 协调增加精确 pane/Agent 匹配、完整 snapshot 缺失、并发 snapshot 顺序、真实重绑 fencing、同步失败不写库且继续 delivery 的测试。
- [x] 1.3 为 Mission status/dashboard 增加 `queued + active` 统计，以及 `working/blocked/done/missing` 展示测试。
- [x] 1.4 为 Mission 详情与角色表格分区、窄窗口核心列和关闭 mouse capture 增加测试。

## 2. Rust 实现

- [x] 2.1 实现 typed Agent snapshot parser 和 `herdr agent list` argv。
- [x] 2.2 实现事务化角色 health 协调，保留 Herdr 状态词汇并保护当前绑定。
- [x] 2.3 实现 `herdr-mission reconcile`，在 health 同步不可用时仍执行现有 outbox delivery，并返回结构化分项结果。
- [x] 2.4 统一 Mission status、角色 `init` 和 dashboard 的未结束 Assignment 统计，补齐 TUI 状态标签与运行计数。
- [x] 2.5 重构 dashboard 详情为 Mission 信息区和带表头角色表格，并保留终端原生鼠标文本选择。

## 3. 插件入口与版本

- [x] 3.1 让 startup 和 Agent 生命周期事件直接调用预编译 Rust `reconcile`，删除未再使用的 `events/reconcile-delivery.sh`。
- [x] 3.2 把 Cargo、lockfile、插件 manifest 和相关文档版本更新到 `0.1.6`。

## 4. 验证与发布

- [x] 4.1 运行 rustfmt、Clippy、完整测试、release build 和 OpenSpec strict validation。
- [ ] 4.2 按 Lore 协议提交并推送 `master`，创建并推送 `v0.1.6` tag，验证三平台资产、`COMMIT` 与 `SHA256SUMS`。
- [ ] 4.3 在本机与 `sqbair` 重新安装 GitHub 插件，验证版本、SHA、doctor、现有 Mission、Reviewer 实时 health 与未结束 Assignment 统计。
