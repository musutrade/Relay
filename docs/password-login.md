# 单用户密码登录

Relay 可以直接用用户名和密码登录，不依赖邮件或 Cloudflare Access。Tunnel 仍只把 HTTPS 转发到本机回环端口；不要直接开放 Relay HTTP 端口。本功能只属于 `app/`，不改变队列内核、MCP stdio 或任务所有权。

## 在运行 Relay 的主机上初始化

以下命令由你在主机终端执行；不要把密码发到聊天、命令参数、环境变量或配置文件中。先升级到含本功能的二进制，并按原操作手册停止/重启服务，不要中断未知的执行任务。

```sh
umask 077
mkdir -p "$HOME/.config/relay"
relay-app auth-init "$HOME/.config/relay/credentials.json" operator
```

终端会隐藏输入并要求确认。密码至少 15 个字符、最多 1024 UTF-8 字节；支持空格和 Unicode。文件只保存用户名与随机盐的 Argon2id 哈希（19 MiB、2 次、1 lane），权限 600，不保存明文。该设置符合 [OWASP Password Storage](https://cheatsheetseries.owasp.org/cheatsheets/Password_Storage_Cheat_Sheet.html) 当前最低建议。新依赖 Argon2/rand_core 负责密码哈希和 OS 随机数，rpassword 负责终端隐藏输入，zeroize 清理应用持有的密码字符串。

在主机创建私有 `auth.json`（credentials_file 替换为真实绝对路径）：

```json
{
  "mode": "hybrid",
  "credentials_file": "/home/YOUR_USER/.config/relay/credentials.json",
  "public_origin": "https://f.mudfishes.com"
}
```

`public_origin` 必须是浏览器地址栏中的精确 HTTPS origin，不带末尾斜线、路径或查询。Relay 不信任任何 Forwarded/X-Forwarded-* 请求头来决定 cookie 安全性或来源。访问别名和本机 HTTP 地址不能代替这个 origin 登录。

保留现有 `RELAY_TOKEN`，增加 `RELAY_AUTH_CONFIG` 指向该 auth.json，然后按原方式启动：

```sh
export RELAY_AUTH_CONFIG="$HOME/.config/relay/auth.json"
relay-app serve /absolute/path/config.json /absolute/path/relay.db
```

使用现有 HTTPS 地址测试：错误密码被拒绝、正确密码登录、刷新仍登录、退出后不能读任务。hybrid 网页使用密码；原 HTTP API 客户端继续使用 bearer token。没有配置 `RELAY_AUTH_CONFIG` 时维持原 bearer-only 行为，仍要求 RELAY_TOKEN，不会自动退化为匿名。

确认新登录可用后，可继续保留 hybrid 用于 API 客户端；如不需要 bearer，将 mode 改为 `session`，从服务环境中移除 RELAY_TOKEN 并重启。session 模式检测到遗留 token 会拒绝启动，避免误以为旧口令已停用。不能把 mode 写成 bearer；旧模式直接不设置 RELAY_AUTH_CONFIG。

## 会话、退出与恢复

- Cookie 为 HttpOnly、Secure、SameSite=Strict、host-only，最长 7 天，绝不自动滑动延期。浏览器刷新/正常重开可以恢复，受浏览器 cookie 保留策略影响
- 会话仅在服务内存中，服务重启会要求重新登录；最多 32 个有效会话，超出时撤销最早到期者
- 页面退出会在服务器撤销当前会话并清除 cookie；网络失败会明确提示，不声称已退出
- `relay-app auth-password /absolute/path/credentials.json` 在本机隐藏输入新密码并原子替换哈希；所有旧会话在后续验证时撤销。忘记密码也使用该主机命令，没有邮件找回流程
- 删除/损坏/放宽 credential 文件权限会使 cookie 访问失败；hybrid 的既有 bearer 凭据仍独立有效。停用该备用通道需改为 session 并删除服务 token 环境变量
- 登录全局最多每分钟 5 次密码验证，最多一个并行验证，不依赖可伪造的客户端 IP。达到限制返回 429/Retry-After；单用户主机重启重置限流。面向公网可另用可信代理限流；应用限流不等于抗拒绝服务
- 所有 cookie 状态写入、登录和退出要求精确 Origin；错误 Authorization 头不会回退到 cookie。API GET 必须无副作用。MCP 仍是可信本机 stdio，不使用网页 cookie
- 不备份会话；保护 credential 文件、其父目录和现有 token，不提交 Git。运行命令的可信本机账户也可以读配置/操作队列，本功能不提供恶意本机代码隔离

仅开发测试可显式设置 `allow_insecure_loopback: true` 且 public_origin 为 http://127.0.0.1:端口（或 localhost / [::1]）；此时使用不同名称的非 Secure cookie。禁止对公网使用该选项；正常配置默认强制 HTTPS cookie。

本变更不自动修改 Cloudflare、Tunnel、系统服务或实际凭据。不要在日志/错误报告中记录登录请求体、Cookie 或 Authorization。
