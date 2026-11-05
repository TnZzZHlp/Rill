# 相对 Caddy 的能力差距

本文记录一次真实 Caddy 站点集合的迁移评估结论，用于说明当前 Rill 无法承接哪些配置，以及若要承接需要新增什么。它是差距清单，不是实施计划，也不代表相关功能已排入 001–012 之外的任何阶段。

## 1. 评估对象与方法

对照对象是一台 Debian 13 设备上的 Caddy v2.11.7 实例：`/etc/caddy/Caddyfile` 加 `sites/*.caddy` 共 17 个文件、25 个站点域名，占用 80/443。该实例使用手工维护的 PEM 证书（`/etc/caddy/certs/<domain>/fullchain.pem` + `key.pem`，叶证书加中间证书，无 ACME），由 Syncthing 同步配置后经 inotify 触发 reload。

判定方法：逐条读取 Caddyfile 实际出现的指令，对照 `src/config/` 的解析器接受集合、`src/proxy/`、`src/static_files/` 的运行时行为，以及 `docs/configuration.md` 的既有契约。只有“Rill 现在真能表达且语义一致”才算可承接；需要改写语义或丢失行为的，记入缺口。

结论：25 个站点中约 12 个可在去掉响应头策略后一对一迁移，其余因缺少下表中的指令无法承接。

## 2. 缺口清单

### 2.1 请求与响应头改写（影响面最大）

Caddy 的 `header`、`header_up`、`header_up -X` 在 9 个站点上用于安全响应头、CORS、缓存策略、上游头删除与身份传递。

Rill 现状：

- `src/proxy/headers.rs` 的 `request` 只做固定动作——剥离逐跳头、剥离客户端自带的 `X-Forwarded-*` 与 `Forwarded`、注入 `host`/`forwarded`/`x-forwarded-for`/`x-forwarded-host`/`x-forwarded-proto`/`x-forwarded-port`。
- `response` 只做逐跳头与 `Connection` 令牌校验，不提供任何配置化改写。
- 全配置没有任何 `header` 类节点；解析器对未定义节点直接报 `CONFIG_UNKNOWN`。
- `cache-control` 是唯一可配置的响应头，且仅限静态动作，不接受代理动作。

受影响能力：安全响应头（`X-Content-Type-Options`、`X-Frame-Options`、HSTS、`-Server`、`-Alt-Svc`）、CORS 预检、上游缓存头清除、`X-Real-IP` 传递、`header_up Host {upstream_hostport}`。

### 2.2 响应合成 `respond`

用于 OPTIONS 预检返回 204、鉴权失败返回 401、站点校验文件返回固定文本、整站返回 404。Rill 的动作集合只有 `proxy` 与 `root`，没有“直接返回给定状态码与正文”的能力，静态 error-page 也不合成对应状态码之外的响应。

### 2.3 路径改写与别名

- `handle_path`：剥前缀后交给 file_server。Rill 的 root 契约是“始终拼接完整站点路径，不剥离路由前缀”，因此 `/yuri/*` 加 root `/etc/api/images/yuri` 在 Rill 里会变成 `/etc/api/images/yuri/yuri/...`。
- `rewrite` / `try_files`：错误页重写、SPA 后备路径。Rill 只有 `spa #true` 这一种固定回退，且仅对 GET/HEAD、`Accept` 含 `text/html`、末段无点的导航请求生效。
- `route` 内顺序执行与 `handle` 互斥：Rill 的路由是“精确优先、最长前缀优先”，与声明顺序无关，语义上等价但不可用于表达“先改写再匹配”。

### 2.4 请求匹配器

Rill 仅支持 `/path` 精确匹配与末尾 `/*` 前缀匹配，且拒绝正则与路径参数。以下 Caddy 用法无对应形式：

- `header` 匹配器：按 `x-auth`、`X-Auth`、`Content-Type application/grpc*` 分流上游，用于 OTel 接入鉴权。
- `method` 匹配器配合 `not`：CORS 之外的按方法分流。
- `path_regexp`：按构建指纹识别 hash 资源并下发长缓存。
- `expression`：`{http.error.status_code} == 404` 形式的 handle_errors。
- `client_ip` / `not`：IP 白名单。

若要在不引入通用表达式的前提下覆盖上述场景，最小集合是“路由级请求头等值/前缀匹配”和“路由级方法集合”，但这是新契约，需单独批准。

### 2.5 上游选择与协议

- **按匹配器选择上游**：Caddy 的 `reverse_proxy @matcher host:port` 允许同一 `handle` 块内不同请求去不同上游。Rill 是“一个路由一个动作一个上游”，必须把匹配条件提升为路由条件（见 2.4）。
- **`h2c://` 与 gRPC 上游**：Rill 的上游固定 HTTP/1（`docs/configuration.md` §2），gRPC over h2c、`application/grpc` 流式调用无法承接。
- **上游 Host 改写**：Rill 固定转发客户端已验证 authority；`{upstream_hostport}` 形式无对应配置。

### 2.6 认证与授权

`basic_auth`（bcrypt/argon2 哈希）与 `webdav` 均无对应能力。当前项目把网关认证列为非目标，但也缺少“由代理按路由校验一个静态凭据”的最小形式。

### 2.7 响应体改写

`replace` 与自建 `dynamic_proxy` 插件用于 URL 重写代理（606 MiB 站点）。Rill 没有正文改写，也不具备流式正文替换的缓冲语义。这类站点不适合迁移。

### 2.8 内容编码

Caddy 的 `encode zstd gzip` 在线压缩；Rill 只有 `precompressed #true`，即使用磁盘上已有的 `.br`/`.gz` sidecar，缺失时回退 identity，不提供动态压缩。对照设备的静态根下没有任何 sidecar 文件，迁移后静态资源将以未压缩形式下发。

### 2.9 日志

Caddy 按站点分文件、1 MiB 轮转、保留 3 份、`filter` 编码器裁剪字段。Rill 只有全局 JSON 访问日志写标准错误（交给 journald），无按站点文件、无轮转策略、无字段裁剪；访问日志不含 URL、请求头与客户端 IP，且默认关闭。

若要用 Rill 替代现有按站点分文件的日志布局，需要 2.10 的站点标签加一个外部日志分流方案（journald 或 Vector/Alloy），这属于部署选择而非 Rill 能力。

### 2.10 可用性标签

Rill 的 Prometheus 指标路由标签是**不透明哈希**（`RouteId` → 16 位十六进制），不是站点名或路径。`rill_route_*` 系列无法直接按域名或路径聚合，排障需要额外的映射来源。指标端点仅接受 loopback 字面量地址且无认证。

### 2.11 通用运行时能力

以下 Caddy 能力没有对应设计，需要单独决策而不是补指令：

- 上游间负载均衡与健康检查（当前为“单请求固定候选，最多尝试 8 个建连失败地址”，是地址级重试而非负载均衡）。
- `push`、`templates`、`intercept`、`map`、`vars` 等中间件。
- 管理 API（Rill 只有 Unix 控制 Socket 上的 reload；Caddy 的 `admin 127.0.0.1:2019` 可用于在线查询/修改配置）。
- 配置语法为非 KDL：迁移必然是一次重写，不能机翻。

## 3. 可承接子集

去掉 2.1 的响应头策略后可以一对一迁移的形态：

- 纯静态站点：单一 root、`index.html`、可选 SPA 与 error-page。
- 静态站点加若干前缀代理：`/api/*`、`/v1/*`、`/health/*` 等显式前缀，其余走静态（对应 Caddy 的 `handle @backend` 加 `handle` 兜底）。
- 纯全站代理：单一上游，含 WebSocket 与 SSE（Rill 显式支持，且 `websocket #false` 会明确拒绝而非静默降级）。

以下语义差异需要在迁移时明确接受：

- 无兜底证书，SNI 与 Host 必须一致，否则握手拒绝或返回 421；Caddy 会宽松处理。
- 未匹配 Host 返回 404，没有默认站点。
- 只注入 `X-Forwarded-*`，不设 `X-Real-IP`；依赖 `X-Real-IP` 的后端需要改造。
- `*.example.com` 只匹配一级子域。

## 4. 若决定补齐的最小指令集

按“能承接对照实例”的目标，建议按依赖顺序分三批，每批都是独立契约变更，需要单独批准并补验证：

| 批次 | 新增能力 | 承接的实例能力 | 风险 |
| --- | --- | --- | --- |
| 一 | `header` 响应头增/删/改（静态与代理动作均可），含 `Vary` 合并规则 | §2.1 全部，是迁移率的最大杠杆 | 头注入与 CRLF 校验、`Vary` 合并顺序需固定语义 |
| 二 | `respond STATUS [body]`；路由级请求头匹配；路由级方法匹配 | §2.2、§2.4、§2.5 的单上游分流 | 匹配器会改变“精确优先、最长前缀优先”的选路模型，需明确组合优先级 |
| 三 | `strip-prefix #true`（前缀剥离）、动态 gzip、按站点日志文件与轮转 | §2.3 的 `handle_path`、§2.8、§2.9 | 前缀剥离会改变 root 拼接契约，与现有“不隐式去前缀”条款冲突，必须显式选择而非默认开启 |

明确不建议纳入：`basic_auth`/`webdav`（非目标中的网关认证）、`replace` 与正文改写、`h2c`/gRPC 上游（需先决策是否支持 HTTP/2 上游，当前上游固定 HTTP/1）、通用表达式匹配器、管理 HTTP API。

## 5. 结论

Rill 当前是“域名路由加代理或静态文件”的服务器，Caddy 在本实例中承担的是应用网关角色。二者不是同一能力层级，无法整体替换。

可行的路径有两条，都需要先明确范围：

1. 保持 Caddy 承担网关类站点，把纯静态与前缀代理站点迁到 Rill 验证生产表现。这条路径不改动 Rill 代码，但需要 2.1 之外的响应头策略在上游应用内重复实现。
2. 立项补齐第 4 节批次一与批次二。完成后可承接对照实例中除 gRPC、WebDAV、正文改写之外的绝大多数站点，但仍无法承接 §2.7 与 §2.5 的 h2c 场景。

在 012 发布收口未完成、且本文缺口未经批准立项的前提下，不应把“替代 Caddy”作为当前版本的目标。
