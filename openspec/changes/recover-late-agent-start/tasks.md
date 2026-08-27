## 1. 回归测试

- [x] 1.1 为启动超时后正确 Agent 接管与稳定名称注册增加失败测试
- [x] 1.2 为 `agent_pane_busy` 接管、无关占用、错误 cwd 和错误区域增加失败关闭测试
- [x] 1.3 为默认三区域名称、初始 tab 重命名和重复恢复幂等增加测试
- [x] 1.4 为旧区域名收敛、busy 不重复启动和已分配 pane 恢复增加复审回归测试
- [x] 1.5 为 session 迟到、旧角色 tab 迁移和固定区域崩溃恢复增加复审回归测试

## 2. Agent 启动恢复

- [x] 2.1 增加结构化 pane 状态命令与解析能力，并覆盖解析测试
- [x] 2.2 在 timeout 和 `agent_pane_busy` 路径实现严格验证、Agent rename 与运行时持久化
- [x] 2.3 确保不匹配和 rename 失败保持 blocked，且恢复不重复启动 Agent
- [x] 2.4 启动前持久化 pane 分配，并在无法识别 busy pane 时失败关闭而不循环启动
- [x] 2.5 未完成 pane 在启动前先尝试接管，并让匹配 provider 但缺 session 的 timeout 状态继续轮询

## 3. Mission 区域

- [x] 3.1 将默认区域精确改为 `工作区`、`审查`、`验证`
- [x] 3.2 将复用的 workspace 初始 tab 重命名为 `工作区`，并保持区域创建幂等
- [x] 3.3 更新执行区域相关注释和断言以使用 `工作区` 术语
- [x] 3.4 让三个已持久化区域全部收敛到固定名称，并将区域工具启动改为可诊断的 best-effort
- [x] 3.5 通过唯一固定名发现恢复未落盘区域，并把旧独立角色 pane 移入工作区域

## 4. 版本与本地验证

- [x] 4.1 将 Cargo package 和 plugin manifest 版本提升到 `0.1.2` 并更新 lockfile
- [x] 4.2 运行格式化、Clippy、完整测试和 release build
- [x] 4.3 验证 OpenSpec change 完整且所有任务已完成

## 5. 发布与远端验证

- [ ] 5.1 使用 Lore commit 提交并推送 `master`，创建并推送不可变 tag `v0.1.2`
- [ ] 5.2 验证 GitHub Release 包含三平台预编译二进制、`SHA256SUMS` 和 `COMMIT`
- [ ] 5.3 在 `sqbair` 更新 Codex integration 并从 GitHub 安装 `v0.1.2` 预编译插件
- [ ] 5.4 在 `sqbair` 验证 version、source、SHA、doctor，并恢复 `rust-version` Mission
