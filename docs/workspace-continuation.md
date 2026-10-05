# 固定工作区与显式继续

一个开发任务链使用一个 `task-<最初任务 ID>/`，修复和显式继续都沿用其中的 `repository/`。启用会话续接的审查也固定使用同目录下的 `reviewer-repository/`，不按轮次新增目录。每次队列执行仍有独立 task ID / generation / owner；队列内核保持不透明的 queued / claimed / finished，不重新打开已经 finished 的记录。

## 普通失败后继续

任务结果为 failure / timed_out / cancelled，且宿主已确认进程结束时，网页显示“继续保留的工作”。确认前请核对失败结果和可能发生的外部操作。该操作保留需求和所选配置，创建一个新的队列任务编号，并引用原工作区；原结果保持不变。若想从头复制源仓库，则使用普通提交表单。

HTTP：

```text
POST /api/tasks/<失败任务 ID>/retry
{"key":"本次操作的稳定幂等键","confirm_stopped_and_reconciled":true}
```

MCP：`relay_retry`，参数同上并增加 `id`。普通 `relay_submit` / `POST /api/tasks` 不接受调用者自行注入 continuation 元数据。每个前置任务最多产生一个后继；重复点击、请求结果不明后重发、不同 key 并发请求和服务重启均返回同一个后继。恢复链失败后，可显式继续最新的失败后继。服务端记录幂等预留后再提交核心任务，避免两个连接各创建一份执行。

列表和详情读取（HTTP / MCP）在原有任务字段外提供 `continuation_status`：无预留为 `null`；已有后继为 `{"successor_id": <ID>}`，网页显示“已续接至任务 #ID”并可打开该任务。该关系来自服务端持久记录，不依赖当前列表是否包含后继或浏览器内存。原任务的结果、状态和编号不变；后继再次失败时，应在后继上继续。

若服务在预留和核心提交之间中断，字段为 `{"successor_id": null}`，网页显示“恢复已预留的续接”，仍需显式确认并通过原幂等入口完成同一次提交。若核心已提交但后继编号尚未回写，读取会按预留的 key 和 payload 找到该后继；读取本身不提交或修改记录。

## 宿主的拒绝条件

- claimed/unknown 不能走此入口。仍须停机核对旧进程及后代、工作区和外部状态，再使用已有 `confirm-stopped-and-requeue`；新的 generation 复用原工作区，但旧 generation 不会再次执行
- 只有工作区 claim 仍精确指向指定前置 task/generation/owner，才允许普通失败续接；竞争或陈旧的续接在执行前失败，不复制一个备用工作区
- 所选源路径、开发/审查 profile、测试、工作流与发布配置必须保持绑定。工作流源 HEAD、固定 base 和宿主 candidate HEAD checkpoint 也必须一致；缺少 checkpoint 或被修改都拒绝，绝不重新 checkout 来“修复”现场
- 已有工作区不重新初始化/复制源文件，不清理未提交文件。缺失、被重定向或初始化不完整的目录要求人工处理
- 发布调用前持久化 publication-attempt 标记。存在该标记就不自动继续，包括结果不明、失败或 dry-run；先在本机核对远端/适配器结果。不要删除标记来绕过核对，本版本不提供自动发布对账或重放
- 缺失供应商会话、改变角色/路径/profile 或协议不兼容，都会明确失败；不会静默换新会话。Codex 在线程建立后、turn/start 前保存线程 ID。已知 ID 的中断会话只能在显式批准的后续工作区 attempt 恢复；同 attempt 中不自动重试。若失败发生在线程 ID checkpoint 之前，本地记录没有可恢复 ID，会明确阻止继续并要求操作员核对，不静默新建线程

工作区独占 flock 随 open-file description 传给 supervisor；父服务崩溃不会在 supervisor 仍存活时释放锁。配置的模型/测试子进程不继承锁。锁可用不是旧执行已停止的证明：supervisor 自身故障仍可能留下未知后代，必须保留核心 claim 并人工核对。PID 文件也不是自动回收授权。

## 保留期

- 继续执行不消耗新的工作区数量名额；开发、审查、Git 元数据与宿主记录共享字节/项数预算。达到 `max_retained_workspaces` 时拒绝新根工作区，不靠删除失败现场腾位
- `successful_workspace_retention_seconds` 是可选项，范围 60–31536000 秒；省略时不自动删除。设置 `604800` 表示成功工作区在七天后可清理。只有核心已持久化 finished + success、最新 owner/generation 匹配且独占锁可取的目录才删除。worker 最多每分钟检查一次，每次最多删除 16 个目录；不会准点保证清理
- failure / timed_out / cancelled / unknown 工作区不会自动按期限删除，供继续或人工备份后清理。数据库历史和幂等记录保留，CLI 自身的会话/缓存也不由此清理。此机制不是全磁盘硬配额

## 升级交接

旧 `task-*-generation-*` 目录没有新的绑定记录，不自动迁移、复制、删除或复用。升级前先停止并核对旧执行，备份 DB、配置和所需代码；旧失败现场（包括未提交文件）保持原样，需可信操作员单独检查和迁移。不能把修改目录名当作完整迁移。部署与现有真实文件迁移不包含在本次离线验收中。

验证使用本机 fake agents，包括保留 17 个未提交文件后续接、跨连接并发和重复请求、HTTP/MCP 一致性、父进程退出后的锁、失败原生 turn 的 ID checkpoint 恢复、基线改变/丢失拒绝，以及成功 TTL 不删除失败现场；不调用真实模型或更改用户主机。
