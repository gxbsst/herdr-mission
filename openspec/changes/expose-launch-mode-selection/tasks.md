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
- [ ] 4.2 按 Lore 协议提交，推送 `master` 和不可变 tag `v0.1.3`
- [ ] 4.3 验证 GitHub release 的三平台资产、`SHA256SUMS` 与 `COMMIT`
- [ ] 4.4 在 `sqbair` 通过 GitHub 插件安装链路安装 `0.1.3`，确认未回退现场 Cargo 编译
- [ ] 4.5 在 `sqbair` 验证版本、doctor、现有 Mission 数据与 Auto 新建行为
