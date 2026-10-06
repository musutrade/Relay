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

## 单次容量不足时显式上调

提交页可在宿主上限内选择本次 `workspace_quota_bytes`。省略或 null 表示使用宿主上限；这种默认任务没有隐藏的上调余量。宿主上限与输入快照上限都不是页面可修改的配置。

操作面板以 `/api/tasks/<id>/operator` 的服务端动作列表为准，显示失败代码、阶段、原因、带时间和完整性标记的当前逻辑用量，以及每个动作允许的最小/最大配额。只有现场及前置配额证据可验证、完整测量可以放入宿主上限时，才允许选择更大的单次额度。`retry` 和 `continue-review` 都可附加 `workspace_quota_bytes`；必须严格大于前置额度及当前用量，且不超过宿主上限。仅继续审查仍须 `revalidate_tests=true`。

动作中的 `quota_increase_required` 明确说明是否必须填写新额度；客户端不从当前用量或错误文本反推。例如已知审查副本的准入需求超过额度时，即使当前目录仍较小，也必须满足服务端给出的最小新额度。已有预留的动作将该字段设为 false：它只恢复已经固定的请求，额度仍以 `reserved_request` 为准，不能再次改选。

例如先用 128 MiB 的单次额度执行、宿主上限为 1 GiB，失败后确认现场可以容纳于 512 MiB，便可在原显式继续请求中加入 `"workspace_quota_bytes":536870912`。后继保存本次额度及紧邻前置任务的证明，重启后仍复用同一目录、脏文件和未提交内容；旧结果及其原配额不被改写。不附加新额度时保留原额度选择（显式值或字段省略），不自动创建提额请求，也不以删除文件腾位。`snapshot_limit_exceeded` 不提供工作区提额补救。

宿主策略在重启间变化时，旧结果和面板仍显示该次实际记录的配额，不把新上限伪装成旧额度；`recovery.inherited_quota_bytes` 单独给出不传新额度时后继的额度。历史 payload 未显式选择配额时继续保持字段省略，并按当前宿主上限执行，以兼容旧行为；显式选过则继续沿用原值。显式提额与已记录的前置实际配额比较，并受当前宿主上限限制。若旧结果既无配额记录也无显式值，实际配额显示为未知，不能凭新上限补造证明。

同一前置任务的首个成功预留同时固定模式、额度选择、测试复验和审查重点。两连接并发提交不同额度、双击或重启重发均返回同一后继，不覆盖首个请求；后继再次失败时只能从该后继继续。预留尚未提交时，操作状态给出原 `reserved_request`，用于恢复该冻结请求。

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

## 审查阶段失败后仅继续审查

原任务已停止，最后一个候选的宿主测试成功，但审查 CLI 失败、耗尽本次 turn 预算或没有返回合法 verdict 时，可选择“仅继续审查（先复验测试）”。该操作仍创建唯一后继并复用固定工作区；不会调用开发 Agent、重新提交候选或进入修复轮次。

```text
POST /api/tasks/<失败任务 ID>/continue-review
{"key":"本次操作的稳定幂等键","confirm_stopped_and_reconciled":true,"revalidate_tests":true,"review_focus":"仅列出本次代码审查的验收重点"}
```

MCP 使用 `relay_continue_review`，参数同上并增加 `id`。`review_focus` 可省略，提供时须为 1–8192 UTF-8 字节；它只影响本次后继的审查提示，不修改原需求、工作流配置、profile、权限或模型调用预算；工作区字节额度只能通过独立的显式提额字段调整。原任务结果保持不变，后继结果的 `workflow.review_continuation` 指向前置任务，`tests` 是本次复验记录，`agent` 为 null。审查轮中的 developer 摘要保留上次开发结果，并不表示重新调用了开发 Agent。

必须显式确认 `revalidate_tests=true`。历史 SHA 和“测试通过”不足以证明外部固定测试、解释器、继承环境、忽略文件或其他输入没有变化；本版本不提供跨执行测试缓存。宿主先核对固定 base、source、candidate checkpoint、HEAD、index 和原始候选文件，再对同一候选运行一次原配置的测试，成功后再次检查候选并继续审查。复验的是原配置的测试命令；若它本身执行完整测试套件，也会照常执行一次。测试失败或候选变化都停止，不自动进入开发或修复。

已返回 changes_requested 的审查不属于“审查未完成”，需要修改代码时使用普通继续。发布已经尝试、结果不明或存在 publication-attempt 标记时仍拒绝，不能通过本入口重放发布。只有原任务已经请求发布且本次候选测试/审查全部通过、此前未尝试发布，才按原有发布门禁继续。

启用会话续接时使用原来兼容的 reviewer session；不静默换新会话或放宽 max_turns、费用、模型和工具权限。每次显式操作是新的有界 CLI invocation，继续沿用配置中的单次预算。会话 ID 缺失、profile/path/role 绑定不兼容时明确失败。旧任务即使没有新的测试证据记录也可使用复验路径，但仍须已有有效的固定工作区 claim、base/candidate checkpoint、已停止结果和需要续接的会话 ID。仅凭上传日志不能保证实际宿主现场已就绪。

普通继续和仅继续审查共享每个前置任务的唯一预留。并发点击不同模式时首个成功预留决定操作；其他请求返回已有后继，不改变其模式或审查重点。刷新、双标签页和不确定请求重发不会产生第二个任务。

## 显式采纳已检查的历史审查

当原审查命令成功退出、同一候选的宿主测试成功，却只因输出格式失败时，可信本机操作员可显式采纳已完整检查的原始响应。此操作不运行开发、测试、模型或原生 CLI 探测，不修改原失败任务；它只创建一个采用原发布选择的后继，交给已有 worker 执行。它不是通用导入、自动审批或测试缓存。

普通审查解析只接受完整 JSON 对象或整个响应仅有一个 `json` 围栏，最多 4096 UTF-8 字节。summary 不再有独立的 512 字节验收限制；阶段摘要仍显示最多 512 字节预览，末轮完整 review 保留在有界结果中。结果超过 16 KiB 时先缩减日志和阶段预览；仅当多轮历史本身仍超限，才缩减较早修复轮的诊断内容并标记 evidence_truncated，末轮结论和发布说明保留完整。approved 仍必须无 findings，changes_requested 仍必须有 findings，字段、重复键和精确候选 SHA 均严格检查。

旧宿主记录只保存 reviewer 预览，不能验证完整终态来源，也不能证明任意测试的外部输入未变化。操作员必须检查真正的原始成功响应（包括所有外围说明、条件和结尾保留意见），确认没有被截断、失败或缺失终态，并明确决定接受前置任务的成功测试而不复验。缺少这些证据时请停下核对，不能将确认字段作为自动绕过措施。新结果会标记 `operator_attested`，不会声称重新运行模型或证明来源真实性。

准备本机 JSON 请求文件（raw_response 必须是完整原文，不能只粘贴围栏中的获批部分；raw_sha256 是这段字符串解码后 UTF-8 原始字节的 SHA-256）：

```json
{
  "key": "review-adoption-unique-key",
  "confirm_stopped_and_reconciled": true,
  "adoption": {
    "candidate_sha": "<完整候选 SHA>",
    "raw_response": "<完整已检查的原始响应>",
    "raw_sha256": "<64 位小写十六进制 SHA-256>",
    "confirm_complete_successful_response": true,
    "confirm_entire_response_reviewed": true,
    "accept_prior_host_tests": true
  }
}
```

```sh
relay-app adopt-review config.json relay.db <失败任务ID> request.json
```

命令只入队并输出 JSON task；使用同一 DB/config 的正常服务 worker 执行。它不提供 HTTP/MCP/网页一键采纳按钮。请求文件上限 16 KiB，原文上限 4096 UTF-8 字节；不接受符号链接或非普通文件。外围说明只能通过此显式动作处理：恰好一个 JSON 围栏，不接受多个对象、多个围栏、错误 SHA、缺失字段、未知字段、重复键或 changes_requested。是否存在文字冲突由操作员检查整份原文，系统不会猜测“第一份 approved”就是最终判断。

提交时将原文与其 SHA-256 放入不可变后继 payload 的 `continuation.operator_adoption.request`，并固定前置宿主结果摘要。原文前 512 UTF-8 字节必须与历史 reviewer 预览一致；这只是局部一致性检查，不是全文来源认证。执行时重新读取宿主结果、核对原绑定和原候选的 HEAD/index/原始文件；启用会话续接时还要求原 reviewer checkout、profile、候选 checkpoint 和上一 attempt 的 completed session 完整匹配。错误、截断、未完成、旧结果被替换、profile/现场漂移均拒绝，绝不自动重新开发或测试来补证。

后继结果的 `workflow.operator_adoption` 包含手工采纳来源、前置任务号、原文/前置结果摘要及接受旧测试的标记。`tests` 保留的是已明确接受的历史宿主证据；`agent` 与新轮次 `reviewer` 为 null，避免伪造调用和重复计量。完整采纳 summary 不参与结果缩减；原文可从该任务 payload 读取，原失败记录始终不变。

采纳与其他继续操作共享唯一后继。相同原文和确认的重试返回原后继；不同原文或先前已经预留另一模式则拒绝。采纳预留后的提交若中断，只能用同一本机 `adopt-review` 请求恢复；HTTP/MCP 普通继续与仅继续审查不能改用或恢复该采纳预留，网页会显示本机核对提示。worker 重启不会重复已结束任务；未知 claim 仍需既有人工停机恢复。原任务未要求发布则不发布；原任务要求发布且当前所有身份/候选检查通过、没有任何先前发布尝试时，才使用原配置的 exact-candidate draft PR adapter。存在 publication-attempt 标记一律拒绝重放；不会自动合并或发布新目标。

## 同一停止阶段更换角色

新版宿主记录包含权威停止阶段时，可在普通继续 `retry` 或仅继续审查 `continue-review` 入口附加单个 `replacement`，显式更换当前 developer 或 reviewer；未提供时仍按原配置和会话继续。它保留工作区与候选，失败修复不重置轮次；新角色创建独立 epoch，旧会话不覆盖。采纳历史审查不接受角色替换，而是继承原任务已经接纳的 reviewer 选择与会话 epoch。细节、权限 challenge 与不支持情况见[在已停止阶段显式更换 Agent](adapter-stage-continuation.md)。
