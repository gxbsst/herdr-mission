## 1. 回归测试

- [x] 1.1 增加 CLI 启动模式优先级测试，覆盖配置继承、显式覆盖与 Manual 回退
- [x] 1.2 增加 `mission-new.sh` 动作测试，覆盖 Auto、Manual、回车继承与非法值失败关闭
- [x] 1.3 运行新增测试并确认现有实现能复现配置不生效与动作不支持选择的问题

## 2. 入口实现

- [x] 2.1 修改 `new` 参数解析，以显式 `--launch-mode` 覆盖一次加载的 `LaunchConfig`
- [x] 2.2 修改“新建 Team Mission”动作，校验并传递单次 Auto/Manual 选择
- [x] 2.3 更新 README，说明启动模式、配置文件和覆盖优先级

## 3. 本地验证

- [x] 3.1 运行 `cargo fmt --check`
- [x] 3.2 运行 Clippy 并将警告视为错误
- [x] 3.3 运行完整测试套件和 release build
- [x] 3.4 核对 diff、插件版本与 OpenSpec 需求覆盖

## 4. 发布与远端验收

- [x] 4.1 将 Cargo 与插件版本同步提升到 `0.1.3`
- [x] 4.2 按 Lore 协议提交，推送 `master` 和不可变 tag `v0.1.3`
- [x] 4.3 验证 GitHub release 的三平台资产、`SHA256SUMS` 与 `COMMIT`
- [x] 4.4 在 `sqbair` 通过 GitHub 插件安装链路安装 `0.1.3`，确认未回退现场 Cargo 编译
- [x] 4.5 在 `sqbair` 运行 Auto canary，记录 fresh pane `agent_pane_busy` 失败与结构化 pane 证据

## 5. fresh pane 就绪修复

- [x] 5.1 增加 fresh pane 第一次 busy、随后成功的 RED 测试
- [x] 5.2 保留历史空 pane 不重复启动的回归测试，并增加归属不匹配失败测试
- [x] 5.3 仅对本次 fresh pane 实现归属验证后的有限 `agent start` 重试
- [x] 5.4 运行格式化、Clippy、完整测试和 release build

## 6. v0.1.4 发布与最终验收

- [x] 6.1 将 Cargo 与插件版本同步提升到 `0.1.4`
- [ ] 6.2 按 Lore 协议提交并推送 `master` 与不可变 tag `v0.1.4`
- [ ] 6.3 验证三平台 release 资产、`SHA256SUMS` 与 `COMMIT`
- [ ] 6.4 在 `sqbair` 通过 GitHub 插件安装链路安装 `0.1.4`，确认预编译资产 SHA
- [ ] 6.5 恢复或重建 Auto canary，验证 PM、Worker、Scout、Reviewer 全部启动且三区域名称正确
- [ ] 6.6 清理临时 canary，并确认 `rust-version` Mission 与 doctor 仍正常
