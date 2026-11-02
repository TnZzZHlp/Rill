# 配置与 CLI 契约

本文以 001 确认的配置语义为基线。002 已实现配置管线与 Clap 驱动的 `rill check`；003 增加明文 HTTP/1.x 的 `rill run`、路由与流式代理。TLS、HTTP/2、静态资源和生产 Reload 仍由后续阶段实现。原始需求保留在 [writing-block.md](../writing-block.md)。

当前可运行 `cargo run --locked -- check --config examples/config.kdl`，或使用 `examples/multi/config.kdl` 检查多文件示例。`cargo run --locked -- run --config examples/config.kdl` 启动 HTTP 服务，需自行提供配置的后端；多文件示例含 HTTPS 上游，只能做阶段性 check，尚不能 run。成功仅表示配置及字面量上游地址策略通过；输出明确标识未检查 DNS、连接及 TLS/静态资源。存在 TLS 定义或静态 root 时返回 `CONFIG_UNSUPPORTED` 和退出码 1，不能将阶段性检查视为完整资源验证。

## 1. 格式与兼容

- 使用 KDL v2，解析器 `kdl = 6.7.1`，仅开启来源跨度功能，不启用 v1 自动回退。
- 布尔值使用 `#true`、`#false`。原文的 `spa true`、`websocket true`、`redirect-https true` 必须迁移；v2 中裸 `true` 为保留值且缺少 `#`，解析器直接拒绝；显式字符串 `"true"` 也不能通过布尔类型校验。
- `{}` 决定层级，缩进不影响语义；保留 KDL 注释支持。
- DSL 不接收属性参数、类型注解或未定义节点；每个节点的参数数量、类型和子节点均须严格检查。数字不接受 NaN/Inf；超时只接受有限正时长字符串，如 `5s`、`500ms`。
- 同一作用域的标量字段只能出现一次；不能用“最后一个胜出”掩盖重复配置。只有明确允许的 `import`、`use`、监听地址、路由、域名列表和 CIDR 策略列表可重复。
- 所有错误包括错误码、文件、节点、行列及原因。跨文件冲突同时给出两处来源；展开节点保留定义与引用链。

## 2. CLI

最终包名与可执行文件名为小写 `rill`，替代原文暂定的 `proxy-server`。CLI 使用 Clap 派生解析；003 已提供 run、check、帮助及版本，reload 尚未提供，调用 reload 返回参数错误退出码 2。以下 reload 用法为后续阶段契约。

```text
rill run --config /etc/rill/config.kdl
rill check --config /etc/rill/config.kdl
rill reload --socket /run/rill/control.sock --timeout 30s
```

- `run`、`check` 的 `--config` 默认 `/etc/rill/config.kdl`；相对入口路径以调用时工作目录为基准，加载后固定绝对路径。
- `reload` 不接受新的配置路径，通知实例重新读取它启动时的入口配置。Socket 默认 `/run/rill/control.sock`；非默认实例使用显式 `--socket`。
- `check` 完整准备候选配置但不绑定服务端口、不发送管理请求、不修改运行实例。
- 成功退出码 0；配置或准备失败 1；命令参数错误 2；管理通信失败、超时或结果未知 3。服务端明确返回失败时退出码 1。
- `reload` 等待真实结果，成功输出请求 ID 与配置代次；超时不承诺操作没有发生，不自动重试。`--timeout` 必须为有限正时长，默认 30 秒。
- 日志写标准错误，成功结果写标准输出，错误不得打印私钥、凭据或完整敏感配置。run 输出受 log-level 过滤的 JSON 行日志；check 保留配置位置诊断文本。
- 003 run 在启动前完成 HTTP 上游 DNS 解析并验证全部地址，连接只使用批准的具体地址，最多尝试 8 个候选；当前地址固定到进程退出。DNS 更新策略留给 006，Reload 留给 007。HTTPS 上游、TLS/root 服务暂报 CONFIG_UNSUPPORTED，不静默降级。
- HTTP 监听为 IPv4/IPv6 独立绑定，IPv6 设置 IPv6-only，可同端口双栈部署；任一监听绑定失败即启动失败，不继续提供部分站点。SIGTERM/SIGINT 都停止接受并等待最多 30 秒排空；正常退出 0，超期中止并退出 1。
- 初始请求头总期限为 60 秒，复用连接下一请求的头部总期限使用上一请求路由的 idle-timeout；总期限不会随每次读取重新计时。已解析请求头最多 64 KiB；框架解析本身仍有约 1 MiB 硬边界。全局最多 1024 个接受容量槽位，包含等待接受的保留槽位。

## 3. 顶层与路径

P0 顶层允许 `server`、`import`、`tls`、`profile`、`site`；P1 增加 `snippet`。尚未实现的功能必须报告“不支持”，不能静默忽略。

```kdl
server {
    http "0.0.0.0:80"
    http "[::]:80"
    https "0.0.0.0:443"
    redirect-https #true
    control-socket "/run/rill/control.sock"
    log-level "info"
}
import "tls.kdl"
import "profiles/*.kdl"
import "sites/*.kdl"
```

- 全配置只能有一个 `server`。允许多个不重复的 HTTP/HTTPS 地址；地址必须包含合法 IP 和端口，IPv6 使用方括号。IPv4/IPv6 共端口须避免双栈重复绑定。
- 至少有一个服务监听；HTTPS 站点必须有 TLS 引用，纯 HTTP 站点可以没有。启用 HTTPS 重定向时必须有 HTTPS 监听且重定向站点可完成 TLS。
- `redirect-https` 默认关闭。自定义重定向端口使用 `redirect-port 8443`；只有一个不同的 HTTPS 监听端口时可自动推导，多端口歧义必须显式指定。443 不附加端口。只对匹配站点重定向，使用规范化站点 Host，不回显非法 Host。
- `control-socket` 与监听地址属于启动配置；更改时 Reload 整体失败并提示重启。`log-level`、`redirect-https`、`redirect-port`、可信代理和上游允许集合随快照热更新；P0 server 除本文列举字段外全部拒绝。
- 相对 `import` 路径以声明它的文件目录解析。相对 `root`、`cert`、`key`、错误页路径也以其定义所在文件为基准；Profile/Snippet 展开不得改用引用者目录。文件来源使用解析后的实际文件路径确定基准；入口保留调用者提供的绝对路径，不固化符号链接目标，以便后续 Reload 能看到入口链接的更新。
- glob 支持逐路径组件的 `*`、`?`、`[ab]`、`[a-z]`、`[!ab]`，不提供转义或递归目录语义；匹配路径按字节序排序。未匹配任何文件报错，避免拼写错误悄悄丢站。支持嵌套导入，基于实际文件身份检测循环，包括符号链接/硬链接别名；同一文件重复导入报错，不重复执行。
- 配置只读取普通 UTF-8 文件。每文件最多 1 MiB，总计最多 8 MiB、512 文件；导入/Profile 嵌套最多 64 层，KDL 结构、块注释和连续 slashdash 注释嵌套最多 32 层，避免解析器栈溢出。加载中核对文件身份及修改元数据，变化时拒绝候选；不承诺替外部文件系统提供跨文件事务。
- 安全策略允许重复 `trusted-proxy "CIDR"` 和 `upstream-allow "CIDR"`，默认无可信代理；上游默认地址范围及强制拒绝项见架构文档。显式 upstream-allow 替换默认允许集合，但不能解除强制拒绝项。
- 不支持环境变量插值或请求变量拼接上游；新增语法须另行确认。

## 4. 域名与 TLS

```kdl
tls "default" {
    cert "certs/fullchain.pem"
    key "certs/privkey.pem"
}
site "example.com" "www.example.com" {
    tls "default"
    proxy "http://127.0.0.1:8080"
}
site "*.example.com" {
    tls "wildcard"
    proxy "http://127.0.0.1:9000"
}
```

- 配置域名为 ASCII DNS 名，可用已经编码的 IDNA A-label，不接受原始 Unicode、端口、URL 或空标签。转小写，移除一个末尾点后校验标签长度、总长度、字符和连字符边界。
- `*.example.com` 仅匹配一个标签，如 `a.example.com`，不匹配 `example.com` 或 `a.b.example.com`。通配符只能在最左侧完整标签。
- 精确域名优先于通配符；同一规范化精确域名或相同通配符重复定义即失败，包括同一 site 的重复别名。精确与通配符重叠不是冲突。
- HTTP/1.1 缺失、重复、畸形 Host 返回 400；HTTP/2 使用合法 `:authority`，若同时有 Host 必须一致。合法 Host 的端口不参与站点匹配，但必须语法有效。IP Host 不匹配 DNS site，返回 404。
- 未匹配的合法 Host 返回 404，无默认站点。Host 不允许空白、用户信息、逗号或控制字符。
- 同一 server 可同时包含纯 HTTP 站点和引用 TLS 的站点；HTTPS 监听至少需要一个 TLS 站点，开启全局 HTTPS 重定向时所有站点都必须有 TLS 引用。TLS 要求合法、已配置且绑定证书的 SNI；缺失或未知 SNI 拒绝握手，不提供兜底证书。握手后 Host 与规范化 SNI 必须相同，否则 421；即便证书覆盖多域名也不允许 HTTP/2 跨域连接合并。移除的站点在旧连接上的新请求返回 404。
- 证书链使用 PEM，私钥使用无交互口令的 PEM；检查私钥匹配、有效期、SAN 域名覆盖和上下文构建。域名覆盖以 SAN 为准，不回退 CN。无法完整覆盖 site 的通配范围时拒绝。
- 手动 Reload 必须重读全部证书和私钥。新握手使用回调开始时选定的单个快照；发布前已开始的握手可完成旧代次，已建立连接不被重新握手或主动关闭。
- 为保证每个新握手都执行证书选择，初版关闭 TLS 会话缓存和 session tickets，禁用重新协商；恢复支持需要另行验证跨代次语义，不能绕过证书更新。

## 5. 路由与静态 root

```kdl
site "app.example.com" {
    route "/api/*" {
        proxy "http://127.0.0.1:8080"
    }
    route "/assets/*" {
        root "/srv/www"
    }
    route "/*" {
        root "/srv/www/app"
        spa #true
    }
}
```

- 路径只支持 `/path` 精确匹配与末尾 `/*` 前缀匹配，不支持正则或路径参数。
- `/api/*` 匹配 `/api/` 和其后代，不匹配 `/api`、`/apix`；如需 `/api`，另加精确路由。`/*` 匹配所有绝对路径。
- 精确匹配优先，其次最长前缀；不按声明顺序选路。百分号转义的十六进制字母统一为大写用于模式比较，但不解码；重复规范化模式报错，转发仍保留原始请求路径。无匹配返回 404。
- HTTP 分帧复用 Pingora 校验；冲突 Content-Length 会拒绝。同时携带 Transfer-Encoding 与 Content-Length 时，框架按 chunked 规范化、移除 Content-Length 并关闭该客户端连接；应用不得因调整超时重新启用复用。
- 每个路由恰好一个动作：`proxy` 或 `root`；不允许多个动作。`spa`、`cache-control` 等静态字段不能用于代理动作。
- site 级 `proxy`/`root` 简写被展开成 `route "/*"`。它可以和不冲突的显式路由并存，但与显式 `/*` 冲突；不能同级同时简写 proxy 和 root。site 级 `spa` 只修饰简写 root。
- 路由匹配以框架校验后的原始 URI 路径为基准，不包含查询串；不隐式解码 `%2F` 或消除点段改变代理路由。非法百分号编码、NUL 和反斜杠拒绝。代理转发保留原始路径、编码和查询串，不隐式去前缀。
- 静态动作在匹配后单独对路径做一次严格百分号解码，拒绝编码的分隔符、NUL、反斜杠和解码后的 `.`/`..` 段，不进行二次解码；使用目录句柄约束的打开方式防止竞态与符号链接越界。
- root 始终拼接完整站点路径，不剥离路由前缀：`/assets/a.css` 加 root `/srv/www` 指向 `/srv/www/assets/a.css`。原文 root `/srv/www/assets` 会指向 `/srv/www/assets/assets/a.css`，应按期望调整为 `/srv/www`；不隐式提供 alias 行为。
- 目录默认查找 `index.html`，无索引返回 404，不列目录。GET/HEAD 之外静态动作返回 405；HEAD 与 GET 状态/头语义一致，但无响应体。
- P0 使用 Last-Modified 实现条件请求，支持 If-Modified-Since/304；提供单个字节范围、If-Range、206/416。多段范围忽略 Range 并返回完整 200，避免隐含 multipart 实现。P1 的各压缩表示需独立验证器。
- SPA 回退仅用于 GET/HEAD、Accept 包含 `text/html` 且最终路径段无扩展名的页面导航。不存在的带扩展名资源、Range 请求、访问拒绝和非法路径不回退。

## 6. Profile 与继承

```kdl
profile "backend" {
    connect-timeout "5s"
    read-timeout "60s"
    websocket #true
}
site "api.example.com" {
    use "backend"
    read-timeout "300s"
    proxy "http://127.0.0.1:3000"
}
```

- Profile 名称全局唯一，可前向引用；`use "a" "b"` 和多条 use 按参数及声明顺序合并，后面的 Profile 覆盖前面的；所有本地显式字段最后覆盖，不依赖字段写在 use 前还是后。
- 允许复用的字段仅为 `connect-timeout`、`read-timeout`、`request-timeout`、`idle-timeout`、`websocket`。禁止复用 TLS、root、proxy、监听、域名、路由和控制权限字段。
- Profile 可引用 Profile，循环及未知引用报错。site 合并后的参数为路由默认值，route 的 use 覆盖 site 默认，再以 route 显式值覆盖。
- 默认 connect-timeout 5s；read-timeout 60s，为上游相邻读取间隔，不是响应总时长；request-timeout 300s，为开始代理到收到响应头的期限，不限制合法流式响应总长度；idle-timeout 60s，为连接无活动期限。WebSocket 默认允许且使用明确配置的 idle-timeout，可按路由调大，不允许无限时长。
- `websocket #false` 时显式拒绝 Upgrade，不能静默降为普通请求。003 中允许的 Upgrade 暂返回 501，显式禁用返回 403；完整 WebSocket 支持归 006。
- request-timeout 从路由选择后开始计入连接、上传及等待最终响应头的总时间，最终响应头到达后不限制流式响应总时长；read-timeout 限制上游相邻读取，idle-timeout 用于下游读写及连接池闲置。连接/上游读取超时返回 504，拒绝连接返回 502；响应已开始后故障只中止连接，不追加第二份 HTTP 响应。

## 7. P1 语法保留与兼容边界

- 完整路由片段使用顶层 `snippet "name" { route ... }`，site 内使用 `use-snippet "name"`，不复用 Profile 的 use。Snippet 内仅允许完整 route，可在 site 中以 use-snippet 链式引用其他片段；不提供参数模板。展开后统一检测循环及路由冲突。
- `server { watch #true }` 显式开启文件监听；默认关闭。watch 的 debounce/队列、日志与指标参数在各 P1 阶段补充，不影响 P0 必须拒绝未知字段的原则。
- 自动监听范围包括入口、导入图及 glob 目录、当前证书与私钥；静态内容变化由缓存失效处理，不触发配置 Reload。
- 预压缩、静态缓存、错误页、目录浏览、Metrics 和 OpenTelemetry 的具体调优参数由对应阶段在既定安全边界内补充。目录浏览、watch、Metrics 网络端点和追踪默认关闭；任何新增字段均须更新规范及合法/非法示例。

## 8. 原文示例逐项对照

| 原文章节/示例 | 处理方式 |
| --- | --- |
| 3.2 site proxy 简写与等价 route | 保留，展开为 /* |
| 3.3 多域名、精确域名、通配符 | 保留，明确通配符只匹配一级 |
| 3.4 多路由与 SPA | spa true 改为 spa #true；按完整路径 root 映射调整 assets 根 |
| 3.5 普通 root | 保留 |
| 3.5 SPA root | spa true 改为 spa #true |
| 3.6 TLS 定义和引用 | 保留，引用必须存在并覆盖全部域名 |
| 3.8 CLI | proxy-server 改为 rill，reload 使用实例 Socket |
| 4.2 import | 保留，确定性排序、无匹配报错 |
| 4.3 Profile 定义 | websocket true 改为 websocket #true |
| 4.3 use 及局部覆盖 | 保留，本地显式值始终优先 |
| 4.4 snippet 提议 | 固定为独立 use-snippet，避免命名歧义 |
| 4.5 代理与静态简写 | 保留，只有与展开后的兜底路由冲突才拒绝 |
| 5 完整配置 | redirect-https true、spa true 改用 #true；通配站点引用的 default 证书必须实际覆盖对应域名，否则 check 拒绝 |

合法语法不代表资源存在或证书有效；002、004、005 负责后续真实配置校验。

## 9. 必须拒绝的示例

```kdl
site "app.example.com" {
    root "/srv/www"
    spa true
}
```

裸 `true` 不符合 KDL v2 语法，必须给出语法错误；即使改成字符串 `"true"`，也必须给出类型错误，不做真值转换。

```kdl
site "api.example.com" {
    proxy "http://127.0.0.1:8080"
    route "/*" {
        root "/srv/www"
    }
}
```

简写展开后两个 `/*` 冲突，必须定位两处来源。

```kdl
profile "bad" {
    root "/srv/www"
}
site "api.example.com" {
    use "missing"
    proxy "file:///etc/passwd"
}
```

分别拒绝不允许继承的字段、未知 Profile 和非 HTTP/HTTPS 上游，不忽略其中任何一个错误。
