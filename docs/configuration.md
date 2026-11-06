# 配置与 CLI 契约

本文以 001 确认的配置语义为基线。002 已实现配置管线与 Clap 驱动的 `rill check`；003 增加 HTTP 服务、路由与流式代理；004 增加 HTTPS、SNI、HTTP/2、证书准备和 HTTPS 重定向。005 增加安全静态文件；006 增加 HTTPS 上游和 WS/SSE；007 已实现原子 Reload 与本地管理。原始需求保留在 [writing-block.md](../writing-block.md)。

当前可运行 `cargo run --locked -- check --config examples/config.kdl`，或使用 `examples/multi/config.kdl` 检查多文件示例。`cargo run --locked -- run --config examples/config.kdl` 启动 HTTP 服务，需自行提供配置的后端；多文件示例含 HTTPS 上游，run 需要可连接且证书受信的后端。成功表示配置、TLS 证书、静态 root 及字面量上游地址策略通过；check 不验证 DNS、连接或上游 TLS 信任，也不监听或写资源。证书读取或验证失败分别报告 `RESOURCE_IO`、`TLS_INVALID` 等定位错误，不能将阶段性检查视为完整资源验证。

## 1. 格式与兼容

- 使用 KDL v2，解析器 `kdl = 6.7.1`，仅开启来源跨度功能，不启用 v1 自动回退。
- 布尔值使用 `#true`、`#false`。原文的 `spa true`、`websocket true`、`redirect-https true` 必须迁移；v2 中裸 `true` 为保留值且缺少 `#`，解析器直接拒绝；显式字符串 `"true"` 也不能通过布尔类型校验。
- `{}` 决定层级，缩进不影响语义；保留 KDL 注释支持。
- DSL 不接收属性参数、类型注解或未定义节点；每个节点的参数数量、类型和子节点均须严格检查。数字不接受 NaN/Inf；超时只接受有限正时长字符串，如 `5s`、`500ms`。
- 同一作用域的标量字段只能出现一次；不能用“最后一个胜出”掩盖重复配置。只有明确允许的 `import`、`use`、监听地址、路由、`use-snippet`、域名列表和 CIDR 策略列表可重复。
- 所有错误包括错误码、文件、节点、行列及原因。跨文件冲突同时给出两处来源；展开节点保留定义与引用链。

## Caddy 动态代理（006 验收进行中）

`dynamic-proxy prefix="/fetch/" anonymous=#true { allow-port 8080; }` 是终止动作；默认仅允许 80/443，额外端口须显式列出。默认 `anonymous=#false`，要求执行路径已成功完成 Basic 或 static-token 鉴权；`#true` 明确接受开放代理及滥用风险。prefix 后当前 URI 的原始 path-and-query 作为整体解码一次，目标仅限 HTTP/HTTPS，拒绝凭据、特殊地址及自身入口。动态请求每次重新解析并检查全部候选后固定 SocketAddr，不跟随重定向，不在发送业务请求后重放。入口认证、Cookie 和转发身份头不会发往目标，响应 Set-Cookie 删除；同源 Location 编码到 prefix 内。每 origin 的 8 条存活物理连接包括握手、空闲池和跨代连接；全进程活动请求上限 128。动态功能尚在独立验收，不代表 006 已完成。

## Caddy 能力补齐 002：规则链与固定响应

本节的 002 是 Caddy 能力补齐阶段，不是前文历史配置管线的 002。站点选择 `chain` 时不能同时声明旧 `route`、`use-snippet` 或站点简写 `proxy`/`root`。链按顺序匹配；没有命中返回 404。`match` 包含一个表达式，支持 `all`、`any`、`not`、`method`、`header-equal`、`header-prefix`、`path-glob`、`path-regex` 和 `client-cidr`。路径 glob 的 `*` 可跨 `/`，`?` 匹配一个字符，反斜杠转义；客户 IP 使用可信代理策略计算，不直接信任入口转发头。`error-status` 只允许在 `handle-errors` 中使用，接受 400–599 整数；不代表所有状态都可进入错误链。

```kdl
site "example.test" {
    chain {
        rule "health" {
            match { path-glob "/health"; }
            respond 200 text="ok"
        }
        rule { respond 404 text="missing"; }
    }
}
```

规则以 `respond`、固定 `proxy`、`serve-files` 或 `branch` 终止，显式 `continue` 才继续后续规则。branch 只执行首个匹配的 `when`，否则执行可选末尾 `otherwise`；没有匹配分支时终止并返回 404。`respond` 状态为 200–599，默认空文本和 `text/plain; charset=utf-8`；204/304 不允许正文，HEAD 不发送正文。固定代理当前仅支持 http1；静态动作使用现有 root 保护并支持 `index`、`spa`、`cache-control`、`precompressed`、`browse`。`header` 支持请求和响应头动作；`rewrite`、`strip-prefix` 和 `try-files` 按链顺序更新当前路径并保留原查询，`reenter` 从链首重新匹配且最多四次。`try-files` 仅对受约束 root 下的普通文件命中，只有缺失才继续下一候选。链尾可声明一次 `handle-errors`：仅本地缺失、方法不支持及本地临时不可用可进入；安全拒绝、容量耗尽、上游响应/传输错误和已提交响应不进入。错误链共享步骤及重入预算，只进入一次；`respond` 不允许 2xx/3xx，错误文件保留原状态且不按 Range/条件请求返回 206/304。Basic 和静态令牌鉴权见下文；其余后续动作暂不支持，不会静默忽略。`{upstream-authority}` 使用经验证的连接目标规范 authority，始终包含端口，IPv6 使用方括号；不使用 DNS 候选地址或 `proxy` 的 `host` 覆盖值。动作执行时尚未确定上游则使用该变量明确失败并返回 503，不填空、不猜目标、不重放规则；延后响应头动作可在代理目标确定后使用，respond/static 无上游时同样失败。正则运行工作区独立限额为全进程 64 MiB；额度不足返回 503。

## 2. CLI

最终包名与可执行文件名为小写 `rill`，替代原文暂定的 `proxy-server`。CLI 使用 Clap 派生解析；run、check 和 reload 均已提供。

```text
rill run --config /etc/rill/config.kdl
rill check --config /etc/rill/config.kdl
rill reload --socket /run/rill/control.sock --timeout 30s
```

- `run`、`check` 的 `--config` 默认 `/etc/rill/config.kdl`；相对入口路径以调用时工作目录为基准，加载后固定绝对路径。
- `reload` 不接受新的配置路径，通知实例重新读取它启动时的入口配置。Socket 默认 `/run/rill/control.sock`；非默认实例使用显式 `--socket`。
- `check` 完整准备候选配置但不绑定服务端口、不发送管理请求、不修改运行实例。
- 成功退出码 0；配置或准备失败 1；命令参数错误 2；管理通信失败、超时或结果未知 3。服务端明确返回失败时退出码 1。
- `reload` 等待真实结果，成功输出请求 ID 与配置代次；超时不承诺操作没有发生，不自动重试。`--timeout` 默认 30 秒，覆盖连接、发送和等待接收；仅接受正整数及 ms/s/m/h 单位，拒绝溢出。等待响应首字节不受帧 5 秒期限提前截断，首字节后帧读取限 5 秒。
- 日志写标准错误，成功结果写标准输出，错误不得打印私钥、凭据或完整敏感配置。run 输出受 log-level 过滤的 JSON 行日志；check 保留配置位置诊断文本。
- HTTPS 上游的身份来自配置 URL，转发 Host 仍保留客户端已验证 authority；采用系统 OpenSSL 信任路径及管理员指定的 SSL_CERT_FILE/SSL_CERT_DIR，不提供跳过验证。DNS 名校验链和主机名，IP 校验链并在发送请求前严格核对 iPAddress SAN，复用连接也必须核验。旧语法上游使用 HTTP/1；chain 固定代理支持 `protocol="h2c"`（仅 HTTP URL）和 `protocol="tls-h2"`（仅 HTTPS URL），完整 gRPC 尚未完成验收。入站可使用 HTTP/1 或 HTTP/2。 上游 CA 候选冻结源文件及 hash 目录，源变化须 Reload；CA 原始材料、可计量副本和元数据计入候选 8 MiB/进程 64 MiB 策略预算，OpenSSL 内部解析、证书对象和惰性缓存不计入该预算，预算不是 RSS 上限。
- run 在启动前解析并检查上游地址；请求触发 60 秒应用缓存刷新（并非 DNS TTL），失败不返回过期地址，失败退避 1 秒。全部候选检查、去重且不超过 64 个；任一禁止地址拒绝整次解析。单请求固定候选，最多尝试 8 个建立连接失败的地址，已建连或已发送请求后失败不重放。Reload 重新准备上游。
- HTTP/HTTPS 监听为 IPv4/IPv6 独立绑定，IPv6 设置 IPv6-only，可同端口双栈部署；任一监听绑定失败即启动失败，不继续提供部分站点。SIGTERM/SIGINT 都停止接受并等待最多 30 秒排空；正常退出 0，超期中止并退出 1。
- HTTP/1.x 初始请求头总期限为 60 秒，复用连接下一请求的头部总期限使用上一请求路由的 idle-timeout；总期限不会随每次读取重新计时。已解析请求头最多 64 KiB；HTTP/1.x 框架解析本身仍有约 1 MiB 硬边界。全局最多 1024 个接受容量槽位，包含等待接受的保留槽位。
- TLS 至少 1.2，握手期限 10 秒，ALPN 优先 h2，其次 http/1.1。协商 h2 后，客户端 24 字节前言另有 10 秒总期限，滴入不重置；读齐即解除，不限制合法长流。HTTP/2 使用 Pingora 默认 100 个并发流和 64 KiB 解码头部限制，连接无活跃流时 60 秒关闭。每流独立执行 request-timeout 总响应头预算；预算耗尽取消该流，不关闭兄弟流、不伪造已发送 504 的日志。HTTP/1.x 对应预算耗尽时尝试返回 504。

## 3. 顶层与路径

顶层允许 `server`、`import`、`tls`、`profile`、`snippet`、`site`。尚未实现的功能必须报告“不支持”，不能静默忽略。

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
- `watch`、`control-socket`、`static-cache-bytes` 与 HTTP/HTTPS 监听地址集合属于启动配置；仅改变地址顺序允许，集合、Socket 路径或 watch 更改时 Reload 整体失败并提示重启。运行时线程和 TLS 基础协议设置固定于代码，没有可配置字段。`log-level`、`redirect-https`、`redirect-port`、可信代理和上游允许集合随快照热更新；P0 server 除本文列举字段外全部拒绝。
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
- 证书链使用只含证书的 PEM，叶证书在前，最多 16 项、1 MiB；私钥为单个未加密 PEM，最多 64 KiB，不交互询问口令。准备全部定义，检查私钥匹配、整链有效期、相邻签名/顺序、服务器认证用途、SAN 覆盖和上下文构建；检测读取期间资源变化。链末项作为管理员提供的准备期锚点，不承诺任意客户端的公共信任。
- 域名覆盖以合法 DNS SAN 为准，不回退 CN；SAN 不接受末尾根点或部分标签通配。证书 `*.example.com` 只覆盖一级子域，通配 site 必须有相同通配 SAN，有限个精确 SAN 不能覆盖整个命名空间。错误不包含 PEM、私钥或口令。
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
- 目录默认查找 `index.html`，无索引且未启用浏览返回 404。GET/HEAD 之外静态动作返回 405；HEAD 无响应体，忽略 Range 并返回完整表示的 200 元数据。
- P0 使用 Last-Modified 和 Date，If-Modified-Since 以秒为精度返回 304；存在 If-None-Match 时不以 IMS 优先。未来修改时间钳位到 Date，无法表示时省略 Last-Modified 并禁用日期判断；同秒更新可能无法区分。支持单个 closed/open/suffix 字节范围及 206/416，非法或多段范围忽略并返回 200；超大数字安全饱和并检查原始十进制逆序。If-Range 仅有效匹配的日期且修改时间距 Date 至少 60 秒时支持 206，近期、无效值或 ETag 返回完整 200。MIME 使用小型明确映射，未知类型为 application/octet-stream；Cache-Control 在准备期校验头值。各压缩表示有独立验证器。
- SPA 回退仅用于 GET/HEAD、Accept 包含 `text/html` 且 q > 0、最终路径段无点的页面导航。仅 Missing 回退；不存在的带扩展名资源、无索引目录、Range 请求、访问拒绝和非法路径不回退。

### 静态增强

完整示例见 [static-enhancements](../examples/static-enhancements/config.kdl)。可用 `rill check --config examples/static-enhancements/config.kdl` 检查；示例监听 `127.0.0.1:18080`，Host 为 `static.test`；`/` 优先返回 index，`/files/` 可浏览。示例在 site 级开启浏览，覆盖整个静态 root 中所有可访问且无索引的目录，会向访问者公开目录名与可见条目；仅在确认这些内容适合公开时启用。初始无 sidecar，回退 identity。运行时默认控制 Socket 需要按控制面规范准备私有目录，实际部署应配置安全路径。

- `server` 的 `static-cache-bytes` 默认 0，范围 0–268435456 字节，是启动项，修改需重启。进程共享正文预算，包含加载中、缓存中及已淘汰但仍由响应持有的正文；仅不大于 64 KiB 的单个正文可缓存，ready 与 loading 合计最多 1024 项，采用有界 LRU。同键正在加载时其他请求直接流式读取。0 时不占缓存池；Reload 后旧正文仍共享固定预算直到释放。预算使用量不是 RSS。
- `precompressed #true` 和 `browse #true` 可用于静态 site 简写或静态 route，均默认 `#false`，不可用于代理，也不是 Profile 字段。预压缩只使用已有 `.br`、`.gz`，源文件必须存在，sidecar 修改时间不早于源文件；不在线压缩。缺失 sidecar 回退可接受的其他表示。
- Accept-Encoding 按质量降序，同质量优先 br、gzip、identity；缺失或空字段使用 identity。重复编码取最低 q，已识别但非法的项视为 q=0；空白只接受 HTTP SP 和 HTAB。identity 隐含 q=1，除非显式设置或 `*;q=0` 拒绝；全部拒绝返回 406。静态文件表示、目录列表及所支持的错误页状态响应以 `Vary: Accept-Encoding` 表示可接受性受其影响，包括 406 及未启用压缩的情况；这不包括路径解码等提前产生的所有 400 响应。MIME 按源路径，Range 按所选压缩表示的字节，HEAD 忽略 Range。
- ETag 是包含表示编码的弱元数据验证器，不是强内容 hash。If-None-Match 使用弱比较或 `*`，优先于 If-Modified-Since；If-Range 不能强匹配弱 ETag，日期仍遵循 60 秒强日期规则。不承诺恶意原位写入时的内容快照。
- `error-page STATUS "path"` 可在 site 或 route 配置，仅支持 403、404、405、406、416、500、503，同状态重复拒绝。相对路径以定义文件为基准，Snippet 保留定义来源。父目录 root 准备失败拒绝候选；叶文件消失、不可读、特殊或超过 1 MiB 时返回空体并保留原状态及 405 Allow、416 Content-Range。HEAD 无体；错误页不递归、不做 SPA、压缩、缓存或条件处理，406 且 identity 不可接受时为空体。仅处理静态响应头发送前的错误，不替代代理错误。
- 显式浏览时索引优先；目录使用稳定 FD 扫描，最多 4096 个原始条目、展示 512 项、HTML 256 KiB，超限返回 503 而非截断。隐藏 dot、越界、magic、特殊及无法 URL 访问的名称；HTML 转义，href 按字节编码。根内安全符号链接遵循 root 契约，挂载子目录通过 mnt_id FD 身份检查。命名空间为可信边界，不承诺恶意挂载或文件系统的全事务。列表无 ETag、日期或 Range；GET/HEAD 的合法 If-None-Match `*` 可返回 304，不存在的错误页不能返回 304。

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
- `websocket #false` 时显式拒绝 Upgrade，不能静默降为普通请求。仅支持 HTTP/1.1 GET WebSocket；禁用返回 403，非法握手返回 400，不支持的协议、CONNECT 和 H2 extended CONNECT 返回 501；框架可能提前拒绝非法 H2 消息。
- request-timeout 从路由选择后开始计入连接、上传及等待最终响应头的总时间，最终响应头到达后不限制流式响应总时长；read-timeout 限制上游相邻读取，idle-timeout 用于下游读写及连接池闲置。连接/上游读取超时返回 504，拒绝连接返回 502；响应已开始后故障只中止连接，不追加第二份 HTTP 响应。

## 7. Snippet 与可选文件监听

顶层 `snippet "name" { ... }` 定义完整路由片段，site 使用 `use-snippet "name"`。每条引用恰好一个名称，可写多条；snippet 内只允许完整 `route` 和嵌套的 `use-snippet`，不允许空定义，不支持参数或模板。Snippet 名称在独立名称空间全局唯一，可前向引用；Profile 仍使用 `use`。所有定义包括未引用定义均检查未知引用和循环；展开后按普通路由规则检查冲突。相对路径以定义文件为基准，错误保留定义和引用两侧的有界去重来源链。整个配置的展开预算为深度 64、复制路由 4096、步骤 16384 和逻辑 payload 8 MiB，超限拒绝候选。

多文件示例见 [`examples/reuse/config.kdl`](../examples/reuse/config.kdl)：两个站点共享路由并各有独立路由。它默认不启用 watch。下面是单独的显式启用形式，完整入口见 [`examples/reuse/watch.kdl`](../examples/reuse/watch.kdl)：

```kdl
server {
    http "127.0.0.1:8080"
    control-socket "/ADMIN_PRIVATE_0700_DIR/control.sock"
    watch #true
}
```

运行前须将占位路径替换为管理员准备的、服务 UID 所有且权限为 0700 的私有目录内路径。`watch` 默认 `#false`，是启动项，修改须重启；`check` 不启动监听。

监听限 Linux 可信本地文件系统：追踪原始入口及符号链接、导入文件、分级 glob 目录、证书和私钥，普通静态内容变更不触发配置 Reload。远端文件系统或挂载变化的事件不保证被观察。监听活动依赖图和最新失败候选的部分图，不无限累积历史；依赖预算为 8192 逻辑项（含已消费绑定）、单路径 4096 字节及总计 4 MiB，每个 inotify FD 最多 2048 个监听目录。无法覆盖时进入有日志的 degraded 状态并持续有界重试；首次注册失败拒绝启动，候选失败保留旧服务。

inotify 内核事件队列以 50 ms 间隔有界 drain、每次最多 32 批，不是文件轮询。事件去抖 250 ms，最长等待 2 秒；BUSY、资源变化和不可用恢复等按 1 秒退避。稳定的普通配置语法错误等待后续修复事件，不无条件重试。队列 overflow 或 watch 失效会记录日志并标记 dirty 重建。自动 Reload 与手动 Reload 共用完整验证、准备和单点提交；注册前后核对实际消费 FD 的指纹，不能将其理解为恶意文件系统上的跨文件事务保证。

## 可观测性配置与运维

`observability` 只允许位于 `server` 中，仅配置访问日志：

```kdl
server {
    http "127.0.0.1:18080"
    observability {
        access-log #true
        access-sample-per-mille 1000
    }
}
```

访问日志默认关闭，采样默认 1000‰；布尔值使用 `#true`/`#false`，采样为 0–1000 的整数。开关和采样随快照发布，已开始请求保留旧策略。运行事件和访问事件共享有界后台日志线程；关闭访问日志并不关闭运行日志线程。`check` 不启动日志 worker 或监听。

[本地示例](../examples/observability/config.kdl) 监听 `127.0.0.1:18080`，Host 为 `observe.test`，开启访问日志。运行前为控制 Socket 配置服务 UID 所有的 0700 私有目录内路径。

Prometheus 指标和 OTLP tracing 已移除。`metrics`、`tracing`、`trace-sample-per-mille`、`otlp-endpoint` 均为未知字段，包括显式关闭的旧字段；迁移时从配置中删除这些节点，再执行 check。服务不创建指标监听、追踪 provider 或 exporter，也不读取 OpenTelemetry 配置环境变量；这些环境变量不再导致 `CONFIG_ENV` 拒启。上游 TLS 的 `SSL_CERT_FILE`/`SSL_CERT_DIR` 行为不变。

访问日志保留本地 `request_id`，不再输出 `trace_id`/`span_id`。代理继续删除传入的 traceparent、tracestate 和 baggage，不注入新的 traceparent。日志尽力送达，队列、写出和关闭边界见[架构](architecture.md#11-访问日志运行边界)。

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

### 鉴权

链中的 `basic-auth realm="private" { user "alice" hash="..."; }` 与 `static-token header="X-Api-Token" file="/secure/token"` 是非终止动作，后面仍须有终止动作、`continue` 或 `reenter`。Basic 支持 bcrypt `$2b$`（cost 10–14）及 Argon2id v19（内存 8192–65536 KiB、迭代 1–3、并行度 1–2）。缺失、重复、畸形或错误凭据返回 401，Basic 附带 challenge；成功后移除认证入口头。令牌文件在准备阶段读取，必须为普通 UTF-8 单行文件、最多 4096 字节且不能被组或其他用户读取；允许一个末尾 LF，不删除其他空白。路径相对声明文件；站点全部链共享令牌头保护，普通头动作不能修改它。

Basic 输入最多 4096 字节；bcrypt 密码超过 72 字节不截断，执行同成本受限 dummy 验证后返回 401，Argon2 不受 72 字节限制。同一请求中同一动作成功后，重入可复用该结果；不同动作不复用。验证全进程最多四个任务，不排队，每次等待最多两秒；超时后底层任务结束前仍占槽。不同用户配置的算法和成本可能有可测耗时差异，不保证隐藏用户名。
