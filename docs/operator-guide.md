# Linux 本机操作手册

面向单个可信 Linux 账户。此文是手动操作说明，不安装服务、不启用 systemd、不配置 sudo/公网/自动部署。需要已安装 Git、固定版本 Rust（见 `rust-toolchain.toml`）、C 编译器和 Python 3。真实 CLI 的安装、账户、模型权限及费用由操作员自行确认；这里的检查不会代办登录。

## 固定提交构建到用户目录

在可信的 Relay checkout 中选择你已审查的完整提交 SHA，不跟随浮动分支构建。下面的占位符必须替换后执行：

```sh
REV='<reviewed-full-40-character-commit-sha>'
test -z "$(git status --porcelain)" || { echo "Checkout must be clean" >&2; exit 1; }
git checkout --detach "$REV" || exit 1
test "$(git rev-parse HEAD)" = "$REV" || exit 1
cargo build --workspace --release --locked || exit 1
umask 077
RELEASE="$HOME/.local/lib/relay/$REV"
DATA="$HOME/.local/share/relay"
mkdir -p "$(dirname "$RELEASE")" "$DATA" || exit 1
mkdir "$RELEASE" || exit 1
install -m 700 target/release/relay target/release/relay-app "$RELEASE/" || exit 1
printf '%s\n' "$REV" > "$RELEASE/REVISION" || exit 1
```

每次新版本使用不同目录，保留上一版本的二进制和配置，不能在运行中覆盖二进制。数据目录应位于可靠的本机文件系统，账户私有，不与源仓库或工作区互相包含。

## 真实 CLI 完整配置

复制 [local-cli-config.json](../examples/local-cli-config.json) 到私有数据目录 `config.json`，将全部占位路径替换为实际绝对路径。测试命令和参数要适配你的项目，例如 Cargo 项目用本机 cargo 路径和 `["test", "--locked"]`。源仓库必须是干净的 Git checkout；工作区应放在单独的私有目录。配置不是 shell，不会展开 `$HOME` 或 `~`。无须配置模型名时由 CLI 选择账户默认模型，不猜测模型 ID。

- Codex 和 Claude 都支持 developer 角色；示例包含 Codex 开发 + Claude 审查、Claude 开发 + Claude 审查两种工作流
- reviewer 当前仅支持具备所需受限只读能力的 Claude CLI；Codex reviewer 是未支持能力，登录成功也不能解锁它
- 示例不配置 PR adapter，job `publish` 默认 false；即使误提交 true，也会因未配置允许项而失败。网页不请求发布
- 显式设置总期限、输出、工作区字节/条目/保留数量和修复次数。Claude 的 2 美元预算是每次 CLI 调用的上限，不是整个任务或账户的总费用上限；多轮开发和审查会累加。Codex profile 没有美元预算字段，需外部账户费用控制

```sh
"$RELEASE/relay-app" doctor "$DATA/config.json"
```

`doctor` 只做版本/帮助检查，`compatible` 不代表认证、模型可用性或审查角色验收通过；真实任务还会检查具体角色。凭据仍由外部 CLI 自行管理。执行下面真实 job 会调用模型并可能收费，先核对账户与范围。

```json
{"key":"project-change-001","job":{"repository":"project","requirements":"实现已确认的需求并补充测试","agent":"codex","workflow":"codex-reviewed","publish":false}}
```

改用 Claude 开发时同时将 `agent` 改为 `claude`、`workflow` 改为 `claude-reviewed`。代码成果位于结果 `workspace` 指向目录中的 `repository/` Git 仓库；工作流结果提供 candidate SHA，JSON 仅是摘要和定位证据，并非完整代码交付。审查确认后可从该仓库提取提交。需要 draft PR 时按 [应用说明](application.md#可选精确-sha-draft-pr-适配器)另行配置和授权发布；本示例不会推送、合并或部署。

## 运行、停止和重启

在持有服务进程的终端运行，口令使用你自己选择的 32–256 字节非空白 ASCII 字符串，不写入仓库：

```sh
read -rs -p 'Relay token: ' RELAY_TOKEN; echo
export RELAY_TOKEN
"$RELEASE/relay-app" serve "$DATA/config.json" "$DATA/relay.db" 127.0.0.1:8787
```

同一 DB 只运行一个 serve。HTTP 默认回环、需要口令；MCP 客户端使用同一配置/DB，仅负责提交和读取，由 serve 执行。远程 TLS 与访问控制须另行配置，此处不开放端口。

前台用 Ctrl-C，后台由持有并核实 PID 的操作员发送 SIGTERM（不要凭陈旧 PID 文件杀进程）。两者均停止领取、请求当前 supervisor 终止并回收进程树，然后等待 worker 保存结果并退出。不能因发出信号就认定停止；要等待该进程实际退出并核对结果。取消被确认后才释放 claim，无法确认的执行仍保留 claimed/unknown。SIGKILL、主机断电或写库失败不保证完成此流程。

重启前核对旧进程及其后代已结束，再用相同配置和 DB 运行相同命令。正常信号停止的运行任务保存为 cancelled，不会自动重做；用新的幂等 key 提交新任务。若 `/api/status` 显示 `recovery_required`，按 [恢复步骤](application.md#状态取消和未知结果)核对实际进程、工作区和外部副作用。不要通过等待、删库、删记录或盲目重排消除未知状态。

## 停机备份与独立恢复

1. 停止所有访问此 DB 的 serve、MCP 客户端及内核 CLI 写入者，暂停提交入口；等待所有操作结束，确认旧执行及后代结束。若无法确认，保留现场并按未知执行恢复流程处理，不把备份当作已安全停机
2. 保持上述静止状态直到备份结束；在没有其他写入者时使用 SQLite backup API，连同 WAL 中已提交数据生成完整独立数据库。不能只热拷贝 `relay.db` 而遗漏 `-wal`。不要手动删除 WAL/SHM
3. 下例使用新建的独立目录；一并保留私有配置、固定版本信息、所需工作区和 candidate Git 仓库（DB 不包含完整代码）。不要把 CLI 凭据或口令加入可分享备份

```sh
BACKUP="$HOME/relay-backups/$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$HOME/relay-backups" || exit 1
mkdir -m 700 "$BACKUP" || exit 1
python3 - "$DATA/relay.db" "$BACKUP/relay.db" <<'PY'
import pathlib, sqlite3, sys
source, target = map(pathlib.Path, sys.argv[1:])
if not source.is_file() or target.exists():
    raise SystemExit('Require existing source and a new backup target')
src = sqlite3.connect(source.resolve().as_uri() + '?mode=ro', uri=True)
dst = sqlite3.connect(target)
try:
    src.backup(dst)
    assert dst.execute('PRAGMA integrity_check').fetchall() == [('ok',)]
finally:
    dst.close()
    src.close()
PY
test "$?" -eq 0 || exit 1
cp "$DATA/config.json" "$BACKUP/config.json" || exit 1
cp "$RELEASE/REVISION" "$BACKUP/REVISION" || exit 1
# 根据 config.json 的实际 workspace_root 路径，在静止期间另行复制所需工作区
```

恢复演练在新的私有目录中使用该备份数据库，不能覆盖运行中的 DB，也不需要原始 `-wal` / `-shm`。先离线 `PRAGMA integrity_check` 和 `relay <restored-db> active/get` 核对任务、结果与幂等记录。恢复配置中改用隔离的工作区根，并恢复所需代码；不要直接启动 worker 去执行备份中的排队任务。备份会保留 claimed 状态，不会证明旧外部执行已停止；真实恢复前仍须核对原进程和远端副作用。旧备份可能遗漏后来已发生的外部动作，重做可能产生重复效果。

## 升级和回退

停机并完成上述备份后，把新版本构建到新 SHA 目录，阅读版本差异和 DB 兼容性说明，使用新二进制 `doctor` 校验配置，再指向相同数据启动。核对 HTTP 状态、历史任务、工作区与新任务结果。保留旧二进制、旧配置和备份，直到验证完成。不要让新旧服务并发访问同一 DB。

若需回退，先再次停止所有写入者并保留失败现场；只有确认旧二进制兼容当前 DB 时才直接切回。否则使用独立备份副本和对应旧版本，核对升级后外部副作用及丢失的任务记录后再启用。这里没有自动迁移回滚或跨版本兼容承诺。

## 容量和日常检查

工作区预算是尽力检查的每工作区边界和保留数量限制，不是全磁盘硬配额；外部 CLI 缓存、构建产物、日志、备份等仍可能耗尽磁盘。定期人工检查文件系统剩余空间、DB（含 WAL）、工作区和外部 CLI 数据大小。SQLite 历史与幂等记录随任务增长；目前没有自动保留期、压缩归档或删除策略。不要擅自删除历史行以回收空间，这会破坏幂等/恢复依据。工作区满时停止接收新工作，先备份并核对任务成果和未知执行，再由可信操作员选择清理；不能清理仍在执行或待核对的工作区。

离线验收覆盖真实 HTTP 服务的 SIGTERM/SIGINT → 假长任务及后代停止 → cancelled 落库 → 同 DB 重启 → 下一任务成功，以及未知 claim 在信号退出/重启后仍保持。它不证明真实账户、付费模型、远端发布或恶意逃逸隔离。
