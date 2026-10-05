# 在已停止阶段显式更换 Agent

普通继续仍保留原配置与会话。只有服务端报告可更换的已停止阶段，才可在同一次显式继续中提交 `replacement`。这是应用与可信宿主功能，不改队列状态机，也不修改主机配置、供应商凭据或已有失败结果。

## 两个有界入口

开发阶段停止后：

```text
POST /api/tasks/<id>/retry
{"key":"stable-replacement-key","confirm_stopped_and_reconciled":true,"replacement":{"profile":"approved-developer","model":{"value":"operator-model-id","source":"manual"}}}
```

审查未完成时：

```text
POST /api/tasks/<id>/continue-review
{"key":"stable-review-replacement","confirm_stopped_and_reconciled":true,"revalidate_tests":true,"replacement":{"profile":"approved-reviewer"}}
```

MCP 对应 `relay_retry` / `relay_continue_review`，额外提供 `id`。`replacement` 复用首次提交的单个 `RoleSelection`，只改变当前入口的角色：retry 只能替换已停止的 developer，continue-review 只能替换 reviewer。模型目录来源、手工模型回退、effort 和原生模式约束与首次提交相同；只校验新选择的目录，不要求重新选择未改变角色的旧目录。

任务的仓库、需求、测试、工作流、发布选择和未改变角色继续继承。单次额度仍只能通过原来的 `workspace_quota_bytes` 显式提额字段增加。相同实际执行设置、仅改目录来源或换一个实际等价的 profile 名字会被拒绝，不能把替换当作重置同一会话的捷径。

开发替换沿用原工作区，包括脏文件与未提交文件。工作流只从宿主确认的原开发轮次继续；例如 round 1、max_repairs 1 的失败修复，替换后仍是 round 1，剩余修复次数为 0。不会从 round 0 重启。改变 developer 后若再次停在开发阶段，后续普通继续也沿用该轮次与原反馈；无法证明停止开发阶段时明确拒绝普通继续，不重置预算。审查替换严格使用原 candidate，先复验原测试一次，再调用新 reviewer；不运行开发或修复。

## 原生权限确认

如果改变后的 developer 模式扩大权限，先预览：

```text
POST /api/tasks/<id>/replacement-challenge
{"action":"retry","replacement":{"profile":"approved-codex","model":{"value":"operator-model-id","source":"manual"},"native_permission":"codex_full_access"}}
```

MCP 为 `relay_replacement_challenge`。返回 `challenge`、到期时间、确认说明和不含秘密的 `scope`；scope 包含 `predecessor_task_id`、`action`、`role` 与解析后的角色设置。正式继续在外层提供 `permission_challenge`，并在 replacement 内提供 `confirm_permission_expansion:true`。Full access / bypass 扩大文件系统和网络访问；确认不会绕过宿主 allowlist 或供应商限制。

首次提交的 challenge 不能授权替换，另一个前置任务、动作或选择的 challenge 也不能复用。未改变角色已经接受的扩大权限不重新弹出确认。新角色的接纳绑定组合后的非敏感解析设置及私有策略摘要，不复用旧任务的整项确认作为新选择授权。

## 现场证明、幂等与会话

每次可替换失败都有宿主写入的 `stopped_stage`，标明角色、原轮次、base/candidate 和剩余预算；不会从错误文本或任务状态猜测。旧结果没有此证明时，明确显示不支持。失败修复保留原输入中的反馈，原文与 JSON 转义后内容均限制在 6144 字节内；无法完整保留时不给出替换入口。新角色收到最多 8192 UTF-8 字节的有界交接，注明其中历史失败、测试和审查信息仅是数据，不是指令；不传递原供应商 ID、完整对话或整个仓库。该证明不随普通日志缩减而被改写；必要证明无法保存时拒绝替换。

同一 immediate transaction 和独占工作区 lease 内，验证不可变前置结果、claim 所有者、现场 checkpoint 与允许的唯一角色差异，再冻结后继。首个成功预留固定所有选择、动作与额度；其他标签页、不同 key、跨连接请求返回该后继。重启或预留/核心提交间隙也恢复同一冻结 payload，不依赖旧 challenge 或目录仍新鲜。不会重新复制现场。后继 payload 超过核心 64 KiB 上限时在预留前拒绝。

新的 `job.role_epochs` 是服务器专用的持久角色身份；每个改变的角色获得随机 epoch 和 `sessions/<role>-<epoch>.json`。未改变角色沿用自己的 epoch；没有 epoch 的旧任务继续使用 `sessions/<role>.json`。旧文件不覆盖，旧绑定和历史保留。跨供应商始终新建 Codex thread 或 Claude session-id，不把旧供应商 ID 交给新适配器；同供应商改模型同样使用新 epoch。后续普通继续沿用这个新 epoch。第一次创建会话的允许标记在 provider 启动前被持久消费；损坏、缺失 ID、缺失已用 epoch 文件均拒绝，不静默新建。

执行前在 lease 下再次验证前置证明、当前策略和 candidate。普通 continuation 仍要求原 reviewer 会话可续接；显式 reviewer 替换可在旧 session ID 缺失时开始新 epoch，但仍要求原候选和 checkout 可证明。当前更换 reviewer 必须保留原 checkout 拓扑：有状态独立审查副本与无状态审查工作目录之间的转换返回 `reviewer_topology_change_unsupported`，不会重置/复制已有 checkout。Codex/astra reviewer 仍为 `review_profile_unsupported`，本阶段不宣称完成未验证的只读隔离。

未知进程、发布已尝试/结果不明、原 host-policy 漂移、被修改的 base/candidate/HEAD/index/原始文件，均保持原有拒绝条件；替换不能迁移主机策略或绕过恢复核对。

## 操作面板

`recovery.actions[]` 的 `ordinary_allowed` 说明是否可普通继续；`replacement` 单独提供 `allowed`、角色、原因、可用 profile 名称和 stopped_stage。只有旧 session 缺失时，两种可用性可以不同。预留状态的 `reserved_request.replacement` 显示真正胜出的选择；无需保存或暴露原 challenge。页面在提交前说明保留原工作、建立新原生会话、继续原阶段；审查操作始终说明复验测试一次。

验证全部使用本机 fake 原生脚本，无真实模型、付费调用、部署或主机权限变更。
