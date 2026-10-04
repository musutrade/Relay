# ADR 0002: 显式应用层，保留不透明内核

状态：已实现

## 背景

用户要求从基础队列继续实现可使用的需求 → 开发 → 测试 → 结果闭环，并提供 HTTP/MCP 与移动尺寸界面。不能把内核初始化或 follow-up issue 当作完整交付。

## 决策

- 新增独立 `relay-app` Cargo package；仍使用原 `relay` 库的 submit/claim/finish，不创建第二套队列
- 开发 schema、allowlisted command profiles、Git/gh adapter、HTTP token、UI 与 Linux supervisor 都属于应用层
- HTTP 默认且仅绑定 loopback；MCP stdio 是可信本机提交/读取入口
- 每个 generation 使用不可覆盖的目录，复制有界源快照，不带源 `.git`；执行器从不会默认修改源仓库
- 取消作为应用元数据持久化；观察 claim 与提交取消使用同一个 immediate transaction，运行句柄匹配完整 claim
- 独立 subreaper supervisor 负责一个命令树，停止和回收有明确确认才 finish；Unknown 保持核心 claimed 并阻塞后续任务
- 模型能力从外部已安装 CLI 获取，默认离线 fake Agent；真实 GitHub adapter 默认 dry-run，明确配置才执行 draft PR

## 代价与非目标

Linux 生命周期实现没有冒充恶意代码安全沙箱；工作区预算检查不是内核磁盘配额；跨服务副作用不保证恰好一次。token 仅提供一个可信账户的入口控制，不是多租户授权。生产远程部署、强隔离、凭据管理和真实模型费用需要单独的环境配置。

Axum/Tokio 增加了应用依赖，但未给内核增加网络或业务概念。没有动态插件、宿主依赖图、通用证据平台或自动 lease 恢复。
