# 原生会话续接

会话由应用适配器与可信宿主管理，队列内核仍只保存不透明 payload / result。升级旧配置不会自动打开会话持久化；先在测试环境验证供应商端兼容性，再显式选择：

```json
{
  "native_agents": {
    "codex": {"provider":"codex_app_server", "program":"/absolute/path/codex", "effort":"medium"},
    "claude": {"provider":"claude_cli", "program":"/absolute/path/claude", "session_continuity":true},
    "reviewer": {"provider":"claude_cli", "program":"/absolute/path/claude", "session_continuity":true}
  }
}
```

## 两种适配器

- `codex_app_server`：真实双向 stdio 协议，依次 initialize、initialized、thread/start 或显式 thread/resume、turn/start。每轮恢复同一 thread ID；线程和 turn ID 必须匹配，成功不能从日志片段推断。恢复请求使用 `excludeTurns:true`，避免回传完整历史。当前验证下限是 Codex 0.160.0，之后版本仍须满足协议，不锁死单一版本
- `claude_cli` + `session_continuity:true`：首轮由宿主生成 UUID 并传 `--session-id`，后续仅传 `--resume <精确 ID>`；不用全局最近会话 `--continue`。模型、effort、环境及预算仍来自可信 profile，审查每轮重新施加 `--restricted`、只读工具与空 MCP 配置
- 旧 `codex_cli` 和未打开续接的 Claude 保留无会话模式，方便分阶段迁移。Codex 只读审查仍不支持，不会用 app-server 绕过限制

app-server 每轮有一个受监管进程。收到成功终态后，宿主结束并回收进程树，下一轮重新启动进程并恢复同一线程；测试期间不保留空闲进程。收到 approval、用户输入或动态工具请求时返回不支持并停止，不自动授权。每个 thread/turn 都设置原有 workspace-write 边界，网络保持关闭；不修改用户配置或凭据。自定义端点、认证、模型接受参数及服务端缓存收益，必须经单独授权的真实烟测确认，离线 fixture 不证明这些能力。

Codex 冷恢复可在 `thread/resume` 响应后回放旧 turn 的 `thread/tokenUsage/updated`，即使设置了 `excludeTurns:true`。仅在恢复已确认且新 turn ID 尚未确定的窗口，适配器保留至多 32 条有界用量快照。用量不能确定 turn ID 或证明成功；跨线程通知、已确定 turn 后的错误 ID，以及旧 turn 的正文或终态仍失败关闭。对应上游测试见 [冷恢复用量回放](https://github.com/openai/codex/blob/main/codex-rs/app-server/tests/suite/v2/thread_resume.rs)。

### Token 用量口径

Codex app-server 的 `usage` 旧字段继续保留 `tokenUsage.last`（最后一次快照），新增 `usage_scope: "last_snapshot"` 与可选 `turn_total`（本轮累计）。`turn_total` 从当前 `tokenUsage.total` 减去恢复窗口内已确认属于旧 turn 的累计基线；新建线程的基线为零。重复累计快照不相加，旧线程历史不算进本轮。缺少恢复基线、缺失某项计数时显示未知，不用最后一次快照冒充累计；计数下降（包括重置或无法区分的乱序）后，本次累计保持未知。失败/中断任务可保留停止前观察到的用量，但它不证明完整账单。协议没有可靠请求次数，`num_turns` 不解释为 API 请求数。

输入/输出 token 是主要指标；Codex 缓存输入与推理输出分别是输入/输出的子集，不再重复相加。旧记录及其他 provider 维持原字段与原始口径，不推断它们一定是本轮累计；尤其 Claude 的缓存读取/创建字段不按 Codex 子集规则处理。费用仅保存 provider 明确报告的可选值，并标为报告费用，不表示订阅实际扣款；未知时不套用 API 单价估算。多轮工作流此处显示所保留阶段的用量，不声称整个任务累计。

累计与缓存字段口径参考 [Codex TokenUsageInfo](https://github.com/openai/codex/blob/main/codex-rs/protocol/src/protocol.rs) 和 [Claude 缓存说明](https://platform.claude.com/docs/zh-CN/build-with-claude/prompt-caching)。字段定义参考 [ThreadTokenUsage](https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/schema/typescript/v2/ThreadTokenUsage.ts) 和 [TokenUsageBreakdown](https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/schema/typescript/v2/TokenUsageBreakdown.ts)。

## 工作区与绑定

同一任务链的修复与显式继续沿用开发 checkout。启用续接的审查使用同一任务内固定 `reviewer-repository/`，只导入精确候选提交和本轮 diff，不复制开发者的 Git 元数据、未提交文件或会话。审查前后同时校验候选；任何候选改变都须重新测试和审查。仍然是可信本机进程管理，不是对恶意程序的 OS 沙箱。

宿主把开发、审查会话分别绑定到 role、规范化 cwd 和 profile 指纹。仅保存有界 ID 与元数据，不复制环境变量秘密；指纹用于防止意外串用配置，不是认证凭据。CLI 自己保存的历史仍受其原有本机存储策略管理。Relay 不读取或代管其登录材料。

执行前先持久化 in-flight 记录；只有供应商成功终态、进程树回收和记录写入全部成功，才标记可继续。首次不存在本地 role 记录时创建新会话；损坏记录、换角色/路径/profile、恢复返回别的 ID 都失败关闭。同一 attempt 的未完成轮次不自动重跑；已知 ID 的中断轮次仅在显式核对后的后续 attempt 续接，详见 [固定工作区与继续](workspace-continuation.md)。不会用新会话静默替代失败的恢复，也不会因等待时间、锁空闲或进程 PID 文件就重排任务。重启后的 claimed 仍须按现有人工恢复流程检查。

## 有界性与验证

- 沿用全任务期限、最多三次修复、工作区字节/项数与保留数量预算；开发和审查目录合并计算
- 单个协议 JSONL 消息最多 64 KiB，摘要最多 4096 字节，ID 最多 256 字节。即使 excludeTurns 避免历史膨胀，包含大量 items 的单轮终态仍可能触及消息上限；此时明确失败，不把截断数据当成功
- stdout/stderr 持续排空，不依赖可见日志长度；超时、取消、协议失败都走现有 supervisor 回收流程。未知进程清理结果保留 Unknown
- app-server 成功后的主动关闭可能显示终止信号；业务成功来自已关联的成功 turn 以及已确认的完整回收，而非伪造进程退出码
- `doctor` 只检查版本和 CLI 能力，不调用模型、不确认 endpoint 支持、不安装 CLI。协议 fixture 覆盖启动/恢复、跨任务/角色隔离、权限请求拒绝、失败/缺失/重复终态、错误 ID、超长流、取消与超时

参考：[Codex app-server 协议](https://github.com/openai/codex/tree/main/codex-rs/app-server-protocol)、[Symphony 客户端](https://github.com/openai/symphony/blob/be10a1b79df723d6d7612b5651c8522704dafb2e/elixir/lib/symphony_elixir/codex/app_server.ex)、[Claude CLI](https://code.claude.com/docs/en/cli-reference)。
