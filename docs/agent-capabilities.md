# 主机 Agent 能力与模型目录

能力面板只列出宿主配置的 `native_agents`，不会扫描整台主机，也不内置模型名称列表。目录是诊断信息，不是模型调用授权；可见模型不保证当前账户可调用，也不证明某次任务实际使用了它。

## 操作与接口

- `GET /api/capabilities` 只读取缓存，不启动进程。每个 profile 返回 `name`、进程级 `cache_epoch`、`generation`、`stale`、`refreshing` 和 `catalog`
- `POST /api/capabilities/<name>/refresh` 显式检查所选 profile 并获取目录，不提交任务或用户推理回合。使用与其他 API 相同的认证和会话请求保护
- 缓存仅存在于服务进程内，五分钟后标记过期；过期不会自动探测。可执行文件或配置变化使旧目录失效，清空旧值并推进 generation。服务重启更换 cache_epoch，前端只在同一 epoch 内比较 generation
- 整个服务同时最多执行一个目录探测；重复刷新不会排队启动更多进程。另一个 profile 正在探测时返回 409
- 进程清理无法确认时停止后续探测，交由可信宿主核对。不能把超时当成进程已停止的证据

目录包含检查时间、来源、版本、兼容性、认证已知/未知、模型与可用 effort，以及不支持或无法确认的原因。缺失 effort 字段是未知，不能当成“没有 effort”。配置的模型不在目录中也不能据此断言调用一定失败，因为供应商可能支持别名或额外路由。

`selection` 区分配置请求值与实际生效值。仅获取目录不会产生任务，所以 `effective_model` / `effective_effort` 保持未知。现有手工配置的模型标识可以继续使用，但没有发现证据时会标为未验证；本阶段不会从面板修改角色、模型或权限。

## 安全与范围

探测使用已配置的 executable 和环境，不读取或复制认证密钥到结果，不新增凭据，不尝试付费推理。返回错误只包含有界的诊断分类，不回传供应商的原始 stderr。初始化本身可能访问供应商服务；不能把“未发起用户回合”描述为“没有任何网络或启动行为”。具体探测上下文和限制必须在目录来源中说明。

当前任务执行仍会重新检查版本和关键参数；能力缓存不会绕过执行前的兼容性、只读审查隔离或工作区/会话绑定检查。Codex reviewer 在完整只读隔离契约获得验证之前保持不支持。此功能不修改核心队列状态，不自动扩大权限，也不启动任务恢复或发布。

## 发现协议与未完成的支持

Codex 使用当前配置 binary 的 app-server `initialize` → `initialized` → 分页 `model/list`，读取运行时返回的模型和 supportedReasoningEfforts。不会发送 thread/start 或 turn/start。它在私有空目录初始化，仍使用原有环境和宿主配置；来源/上下文会明确说明这点。[官方协议](https://learn.chatgpt.com/docs/app-server#list-models-modellist)

Claude SDK 的初始化响应确实提供 models，且无需发送用户推理回合。但本阶段没有启动该目录探测：restricted 模式仍可能加载受管理的 hooks，普通 disableAllHooks 设置无法覆盖组织策略。这里报告的是“启动隔离尚未验证”，不是断言用户机器存在这些 hooks、账户被拒绝或 CLI 不支持发现。不会换用官方 API 账号目录冒充 CLI 订阅目录。现有配置中的手工 model ID 保持可用并标为未验证；Claude 自动目录是后续待完成的支持。[官方 hook 优先级](https://code.claude.com/docs/en/hooks#disable-or-remove-hooks) · [SDK 初始化实现](https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/query.py)

## 只有进程状态未知时才需要核对

普通协议失败和已确认结束的超时会自动释放发现锁，不需要更改文件或重启服务。如果服务异常退出或进程树结束无法确认，先由可信宿主核对保留的发现进程与诊断，再执行：

```sh
relay-app doctor <config.json> --confirm-catalog-stopped
```

此命令声明操作员已确认之前的发现进程树停止，仅清除对应的恢复标记，不杀进程、不推断停止、不调用模型。仍在运行并持锁的发现操作会被拒绝；服务随后允许重新刷新，无需手工删除文件。不要在未核对进程时使用这个确认选项。
