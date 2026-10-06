# ADR 0003：区分严格审查与原生本地沙箱审查

状态：已采纳

## 背景

用户需要独立选择 Codex/Astra 作为 reviewer。Codex 原生命令沙箱不等于无执行工具，也不能隐含约束原生启动 hooks、MCP、插件、Apps 或远程执行器。直接删除 Claude-only guard 会错误继承严格契约。

## 决定

- 保留 Claude 严格审查；新增默认关闭、宿主允许并逐次明确确认的 `codex_native_sandboxed_review`
- 仅对本地命令要求 read-only / networkAccess false / approval never；原生启动与集成是明确披露的操作员信任边界，不声明全局只读或启动隔离
- 保留 native settings、认证、模型目录与模型元数据；不通过复制凭据、更换 provider 或伪造模型能力获得表面兼容
- 首版固定 Codex app-server 0.160.1，执行前核对关键策略快照，缺失即拒绝；只使用 fresh ephemeral 线程，provider resume/fork 不支持
- 独立 reviewer checkout 与 provider 会话保留分别判断；复用既有候选、测试、verdict、supervisor、角色 epoch、资源和 continuation 边界，不增加核心状态或第二个工作流引擎

## 代价

可信集成可能在本地沙箱之外产生副作用；事后候选完整性校验不具备预防或回滚这些副作用的能力。每次 reviewer 调用都失去旧对话上下文。更高版本需要重新核对契约；离线测试不能替代真实主机授权验收。旧的严格 Codex 无执行契约仍未获得支持，不能将本档完成写成原验收项完成。

操作与验证见[原生沙箱审查](../native-sandboxed-review.md)。
