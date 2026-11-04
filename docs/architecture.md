# Rill 架构与实施基线

本文以 001 的模块与接口契约为基线，并记录 002–007 已交付边界。配置格式见 [configuration.md](configuration.md)，服务验证见 [003 验证记录](validation/003-http-routing-proxy.md) 、[004 验证记录](validation/004-https-certificates.md) 与 [005 验证记录](validation/005-static-files.md)。007 验证见 [验证记录](validation/007-atomic-reload.md)。

## 1. 目录与职责

当前 main 仅委托 `cli::run`，CLI 使用 Clap 解析参数，check 调用统一配置构建入口。002 已提供 config 模块，003 已提供 routing、proxy、runtime 和结构化日志。001 的技术验证仍隔离在 tests/feasibility/；003 的真实服务验证在 tests/http_proxy/，004 的 TLS/HTTP2 验证在 tests/https/。004 已增加 tls 模块，005 已增加 static_files 模块，007 已增加控制面，可观测性由独立模块实现，不在 main 或 CLI 中堆业务逻辑。

| 拟议模块 | 负责 | 不负责 |
| --- | --- | --- |
| src/main.rs、src/cli/ | 参数、生命周期编排、结果和退出码 | 配置语义、匹配、证书操作 |
| src/config/ | loader、parser、Profile/Snippet 标准化、来源跨度、语义验证 | 发布活动状态、HTTP 协议解析 |
| src/routing/ | 不可变 Host/路径匹配表与冲突检测 | 上游连接和文件打开 |
| src/runtime/ | 完整候选准备、单点提交、代次和旧资源生命周期 | 管理传输协议 |
| src/tls/ | PEM 检查、证书集合、SNI 回调及握手身份 | 独立发布第二个活动配置 |
| src/proxy/ | Pingora 适配、上游策略、头部和流式处理 | 自行实现 HTTP/TLS 栈 |
| src/static_files/ | 根目录约束打开、表示选择、条件/范围响应与可选缓存 | 重新解析配置 |
| src/control/ | Unix Socket、对端授权、帧限制、请求结果 | 绕过 runtime 验证 |
| src/observability/ | 结构化运行事件与访问日志 | 改变代理业务结果 |

001 验证目录：`tests/feasibility.rs` 为测试入口，`tests/feasibility/configuration.rs` 验证 KDL 契约，`snapshot.rs` 只准备内存快照与临时证书，`tls.rs` 使用 Pingora 的真实异步握手流程。没有模拟 TLS，也不写入私钥文件。

## 2. 配置构建契约

002 在 src/config/ 实现下列加载至准备边界；007 已交付 RuntimeSnapshot 发布。解析、导入、继承及验证不依赖 Pingora 类型：

```text
load(entry) -> LoadedConfig
parse(loaded) -> ParsedConfig
normalize(parsed) -> NormalizedConfig
validate(normalized) -> ValidatedConfig
ResourcePreparer::prepare(validated) -> Resources
build(entry, preparer) -> PreparedSnapshot<Resources>
publish(prepared) -> Generation
```

- `ParsedConfig` 保留每个文件、节点跨度、导入关系；错误来源不能在展开时丢失。
- `NormalizedConfig` 消除简写、应用继承，仍不包含 Pingora 对象。
- `ValidatedConfig` 保证字段类型、域名与路径冲突、引用及安全策略一致；验证不是资源存在性的替代。
- 完整服务的 `PreparedSnapshot` 必须持有路由、上游策略、root 句柄与已验证 TLS 材料，准备失败不产生发布副作用。005 以同形 PreparedRoute 区分 Proxy/Static，准备 root 和 PreparedTls。
- `StartupSettings` 保存监听、控制 Socket、watch 启动开关、运行时线程和 TLS 基础协议设置；后两者固定于代码，没有可配置字段。HTTP/HTTPS 地址按集合比较，顺序重排允许。Reload 必须和启动值比较；任何需要重启的差异整体报错。
- 002 check 使用 build → load → parse → normalize → validate → ResourcePreparer，同一个入口供后续 run/reload 复用，不另写验证管线。check 不监听、不发请求、不发布。check 与 run 共用 root 和 TLS 准备；check 不检查 DNS、连接或上游 TLS 信任，不监听或写资源。证书资源检查与 run 共用 PreparedTls，不单独维护一套规则。
- 新配置依赖的文件必须在本轮准备时读取并保持必要资源，不能发布后再懒加载证书或 root。
- 文件读取过程中可能发生外部编辑；准备期检测所读取文件身份/元数据变化并拒绝不稳定候选。管理员需要跨文件内容事务时应先准备完整版本再触发 Reload，系统不承诺替外部文件系统提供事务。

## 3. 单根快照与原子发布

RuntimeSnapshot 包含标准化配置、Router、准备好的代理或静态路由、PreparedTls 和日志策略。以下为快照内容：

- 单调递增的 `Generation`；溢出时失败而不是回绕。
- 站点及 Host/路径路由表。
- 上游端点、超时、转发头和访问控制策略。
- 静态 root 句柄及静态响应策略。
- 同代次的 SNI 证书集合、私钥及验证结果。

`RuntimeState` 拥有唯一 `ArcSwap<RuntimeSnapshot>`。准备时不写活动状态，管理提交串行化：读取配置、准备全部资源、比较启动设置、赋代次、一次 store，随后返回成功。关闭和提交共用短暂的 std Mutex gate，store 前检查关闭状态和 30 秒期限；blocking closure 只构建，不能访问 state 或 store。代次 checked_add 防溢出。启动快照使用后释放，不永久持有第一代；运行期 App 日志读取当前快照。失败没有发布，因此所谓回滚是保留旧快照，而不是先局部更新再反向恢复。

Snippet 在标准化时展开，定义和引用来源有界去重保留；引用深度、复制路由、步骤及逻辑 payload 使用全配置共享预算。具体限制见配置文档。

watch 使用独立于快照的 inotify FD 生命周期，最多活动和候选各一个 FD，快照不持有 FD；切换时交接 dirty 事件。自动及手动 Reload 共用 admission/execution permits 和提交 gate，均经过完整候选准备和单点发布，不能绕过关闭或期限检查。注册前后核对实际消费 FD fingerprint；溢出、失效和不可覆盖分别触发有日志的重建或 degraded 有界重试。仅信任本地文件系统及既有 proc/namespace 边界，不承诺恶意或远端文件系统的事务及事件保证。

只有管理提交路径可以串行化。HTTP 请求不持有全局管理互斥锁；通过 `load_full` 获取 Arc。路由、TLS、root 和上游不得各自维护独立活动指针。旧快照由持有者自然释放，不能在发布时关闭旧连接。

### 003 HTTP 运行适配

- 使用经用户批准的 socket2 0.6.5 安全绑定 TCP，IPv6-only 避免双栈占用冲突，绑定失败返回启动错误。全部地址绑定成功后才进入接受循环。
- Tokio 负责接受、信号和连接 JoinSet，接收后交给 Pingora Listener/HttpServerApp/HttpProxy；HTTP 解析、流式背压和连接池不重复实现。框架高级监听构建是私有接口，不依赖该接口或 expect 绑定失败。
- 全局 1024 个接受容量槽位（包括保留待接受槽位），单监听 backlog 1024。SIGTERM/SIGINT 先停止接受并通知 Pingora cleanup，最多 30 秒排空；超期取消连接任务并退出 1。
- App 在每请求外层维护 Ingress/Headers/Streaming 预算。首请求头总期限 60 秒，后续头部期限用上一请求路由的 idle-timeout；request-timeout 包含连接、上传和最终响应头等待，不能由慢速滴入不断延后。最终响应头后只保留流式读写/空闲限制。
- SharedStream 的锁只属于单个连接、只在一次 poll 内持有，不跨 await。预算到期先取消原代理请求，再安全写入有限错误响应，不因取得错误响应能力引入全局互斥或后台请求继续执行。
- DNS 应用缓存由请求触发 60 秒刷新，不代表真实 DNS TTL；失败不返回过期地址，失败退避 1 秒。全部候选检查去重且不超过 64 个，任何禁止候选拒绝整次解析。每请求固定 Arc 候选，最多 8 次建立连接失败尝试，已建连或已发送请求后不重放。每路由 Mutex 只串行刷新，不以全局锁串行热路径；最多 16 个实际阻塞 resolver，许可由真实 spawn_blocking 任务持有，取消等待不发布迟到结果。

## 4. 握手、请求、连接的代次

### TLS 握手

1. 004 的 Resolver 通过 Pingora `handshake_with_callback` 安装 `TlsAccept`（001 验证的 `TlsSettings::with_callbacks` 使用同一扩展点），证书回调在开始选证书时读取一次活动快照。
2. 从该快照按规范化 SNI 查找并安装证书、链和私钥。完整准备已验证材料，回调不做文件读取。
3. 使用 OpenSSL 安全的 `Ssl::new_ex_index` / `set_ex_data` 保存选中的 Arc。不能在握手完成时重新 load 当前配置推断证书代次。
4. `handshake_complete_callback` 把同一选择传入 `SslDigest.extension`，HTTP 层可以查询握手身份。缺少身份或安装失败则拒绝服务，不能退到其他站点。
5. 初版关闭 session tickets、会话缓存和重新协商，保证新握手执行选证书回调。恢复 resumption 需要独立验证证书更新约束。

`TlsAccept::certificate_callback` 不返回 Result。未知 SNI 不安装证书；监听基础上下文不得有兜底证书，从而使握手失败。安装错误必须记录脱敏错误并标记身份无效；HTTP 层不得接收没有有效选择身份的连接。001 的未知 SNI 用例验证空基础上下文不会提供其他站点证书。

### HTTP 请求

- 在最早可获得完整请求头的请求过滤阶段读取一个快照，并存入每请求 CTX；后续所有过滤器、动作、上游选择和响应策略都用该 Arc。
- HTTP/1.1 Keep-Alive 的每个新请求、HTTP/2 的每个新流各自读取当前快照；不把整个旧连接永久绑定旧路由。
- TLS 连接的证书和握手身份不可追溯更新。新请求按当前快照检查 Host 仍存在，并要求 Host 与握手 SNI 一致；移除站点后返回 404，不跨站回退。Host/SNI 不同返回 421。
- 允许已开始请求和发布前开始的握手完成旧代次。握手和请求是两个不同时间的操作，并不承诺旧连接上的当前路由对应一份重新握手的新证书。
- WebSocket 升级后的双向流、SSE 和大文件响应持有开始请求的资源直到结束；Reload 不主动终止。

### 原子性的准确边界

发布对整个快照线性化。发布后开始选择的新握手和开始处理的新请求读取新快照；已经取得 Arc 的操作继续旧快照。不能把“旧连接仍用旧证书”误报为半发布，也不能让一次新操作从不同活动指针拼接路由和证书。

001 验证的是这一机制及 Pingora 扩展点，不是完整 HTTP 路由、HTTP/2 流、WS/SSE 或 Reload 管理接口的端到端实现；后者分别由 003、006、007 验收。

## 5. 代理信任边界

- 客户端不能决定上游 URL。仅允许管理员配置的固定 HTTP/HTTPS origin；拒绝 userinfo、fragment、查询串和非根 base path，不隐式重写路径。
- Host 只用于选择站点，不能直接成为连接目标。默认向上游发送已验证的原始 authority/Host，HTTPS 上游 SNI 和证书验证名称则来自上游 URL；两者不可混淆。HTTP/2 `:authority` 与 Host 的一致性由统一适配层处理。
- HTTPS 连接只使用已校验 SocketAddr，信任标准系统 OpenSSL 路径，管理员进程环境的 SSL_CERT_FILE/SSL_CERT_DIR 可影响信任。DNS 名使用框架链及主机名校验，不设置替代 CN。IP 启用链验证并设置非空 sni 以避免框架禁验证，OpenSSL 对 IP 不发送 SNI；仅替换框架的错误 DNS 名检查，以 hook 严格匹配 iPAddress SAN 字节，connected_to_upstream 在新建及复用连接发送任何 HTTP 字节前核验具体 IP，缺失或不符返回 502，不入池、不重试。上游固定 HTTP/1，入站仍支持 HTTP/1 和 HTTP/2。
- 上游仅允许已批准的解析目标。默认允许配置显式指定的 loopback、RFC1918、IPv6 ULA 和合法公网单播；永远拒绝 unspecified、multicast、link-local（包括云元数据地址）、IPv4-mapped 绕过及非单播特殊地址。IPv4-mapped IPv6 先折叠为 IPv4 再检查，正常映射地址按对应 IPv4 策略判断。
- `server` 可用重复 `upstream-allow "CIDR"` 收窄允许集合；显式提供时替换默认允许集合，不覆盖强制拒绝项。域名需检查全部候选解析地址，建立连接时只能使用通过检查的地址；重新解析也要检查，不能校验后再按域名二次连接产生 DNS 竞态。
- URL、地址和 CA 检查在配置准备和实际地址更新时执行。内网后端属于明确需求，不一概禁用；受保护元数据服务仍禁止。
- 默认不信任任何转发代理。删除客户端 Forwarded、X-Forwarded-For/Host/Proto/Port 后，根据已验证 Host、真实对端和当前 TLS 状态重新生成。
- 如使用 `server { trusted-proxy "CIDR" }`，仅当直接对端在允许集合内才解析其 X-Forwarded-For；从右到左移除可信跳点，首个不可信地址为客户地址。格式无效则拒绝请求，不能保留原始字符串。重新生成 Forwarded/X-Forwarded-*；Proto 只允许可信直接对端给出的单值 http/https，Host 始终以已验证 authority 为准。
- 清理标准 Hop-by-Hop 字段及 Connection 指定字段；Upgrade 仅在已验证的 WebSocket 路径显式恢复握手头。仅 HTTP/1.1 GET、规范 16 字节 base64 key、version 13、Connection Upgrade 且无请求体；禁用 403，坏字段 400，不支持协议、CONNECT 和 H2 extended CONNECT 为 501，非法 H2 消息可能被框架提前拒绝。不提议扩展，拒绝上游未提议扩展；子协议必须是提议内单选。101 必须通过 Accept、Upgrade、Connection 和 framing 校验，意外 101 拒绝。报文长度冲突和协议解析依赖 Pingora，复核框架行为，不另写宽松解析器。CL+TE 会规范化为 chunked 并强制关闭客户端连接，应用不能为设置超时重新启用已被框架禁用的复用。
- 流式背压、有界头/体缓冲、有限连接/读取/空闲超时是 P0。默认不自动重试已发送到上游的请求，尤其不能重复执行非幂等写入。

## 6. 静态文件边界

- Linux root 以 O_PATH 目录 FD 持有；准备时探测 openat2 BENEATH/NO_MAGICLINKS，核对 /proc/self PID、procfd 的 procfs 类型和自身 FD，能力不可用则 fail closed。允许约束解析的根内相对符号链接，拒绝绝对、越界和 magic link。
- 客户路径先以 O_PATH 打开并检查普通文件或目录；普通文件经持存可信自身 procfd 的数字入口重开并比较 dev/ino/type，原 FD 保持存活，避免设备/FIFO 先读开和 FD 重用竞态。索引和 SPA 均从原 root FD 重开。路径严格解码一次，使用完整站点路径，不去路由前缀。
- 信任本地文件系统、procfs 挂载/namespace 和进程 FD 表；不保证恶意挂载、FUSE 或 NFS 不阻塞，不替代 OS hardlink 和内容发布权限，也不声称消除所有文件系统竞态。
- 全静态路由跨候选和配置代次共享进程级 128 个 Semaphore 许可，随 Opened 和 blocking 任务持有；max blocking 为 16，单块不超过 64 KiB，逐块背压且不超过声明长度。文件截断则中止流，不追加 HTTP 响应。最终头之前同步交接 Headers → Streaming 和 response_started，本地错误及重定向也如此；正常排空最多 30 秒，runtime.shutdown_background 避免未完成阻塞 IO 卡住运行时析构。
- 每次请求安全打开并核验 dev/ino、size、mtime/ctime 纳秒、mode、uid、gid；缓存命中也 fstat 比较版本，加载前后 fstat 变化返回 503。这不是恶意原位写入的事务快照。
- 进程级正文缓存预算包括 loading、ready 和淘汰后仍被响应持有的正文，响应使用切片 owner 而非复制；Reload 共享该固定预算。`RuntimeState::static_cache_stats` 是只读库 API，不是 HTTP 或 CLI 端点，使用量不是 RSS。预压缩、错误页及有界目录浏览的配置和响应细节见配置文档。

## 7. 本地控制协议

- 使用 AF_UNIX 流 Socket，默认 `/run/rill/control.sock`；父目录 0700、Socket 0600，归服务 UID。拒绝不安全目录或链接，不误删其他实例 Socket。部署由 systemd RuntimeDirectory 或等效方式准备目录。
- 按目录 FD 使用 openat2 禁止 symlink/magic link；祖先须为 root 或服务 owner，若可写须 sticky。信任 proc 挂载、namespace、服务 UID 和 root 写入者；缺少安全内核或 proc 能力时 fail closed。私有目录使用短临时名和能力 FD 绑定，验证身份/权限后 renameat2 RENAME_NOREPLACE 到最终名；既存 Socket、文件及链接一律保留并失败，不自动清理。管理员确认无实例后手工处理遗留。正常 Drop 仅在 dev/ino/uid/type 匹配时 unlink；检查与 unlink 非原子，不保证抵御同 UID/root 恶意写入。绑定后首次 inode 检查失败可能保守留下临时项。
- 接受后检查 SO_PEERCRED，默认仅服务 euid 和 root 可调用；Socket 文件权限不是对端身份检查的替代。初版不提供组级授权或公网管理端口；最多 32 个会话。
- 单行 UTF-8 JSON 帧，以换行结束，每帧含换行最多 64 KiB；每连接一条请求一条结果，帧读取和写入各有 5 秒总期限。CLI 等待响应首字节由整体 timeout 约束，首字节后帧读取限 5 秒。未知字段、重复 JSON 键、未知版本和超限报错并关闭，不无界读取。

```json
{"version":1,"id":"operator-001","operation":"reload"}
```

请求 id 为 1–64 位 ASCII 字母、数字、短横线或下划线。返回同一 id；不将 id 当作授权或幂等凭据。

```json
{"version":1,"id":"operator-001","status":"ok","generation":2}
```

```json
{"version":1,"id":"operator-001","status":"error","code":"CONFIG_TYPE","errors":[{"file":"sites/app.kdl","line":4,"column":5,"item":"spa","reason":"expected boolean"}]}
```

- 成功必须在 store 完成后回复；错误为未提交、保留旧代次。结果长度限制 64 KiB，超出的诊断按条数截断并返回 truncated 标志，日志只记录静态 code、id 和 generation，不记录原始帧。
- 运行实例只读取自己的入口配置，不接收任意路径、PEM 或上游 URL 管理操作。
- 一次仅执行一个准备/提交，最多再排队一个请求，其余返回 BUSY；watch 使用同一入口并合并事件。运行中的准备以 30 秒为默认期限，超时或关闭时不得晚到发布；admission 为 2（执行 1、排队 1），execution 为 1；两个 OwnedPermit 由异步提交路径和真实 blocking closure 通过 Arc 共同持有，超时或取消不能提前释放真实任务额度。读完请求后准备期间 EOF 或多余字节取消管理 future；最终 gate 防止关闭或过期后发布。
- CLI 等待超时或连接断开可能代表结果未知；不撤销已经完成的提交，不自动重发。服务端日志以 id 和 generation 提供追溯。

## 8. 错误分类

| 错误码 | 含义 | 状态影响 |
| --- | --- | --- |
| CONFIG_SYNTAX / CONFIG_TYPE / CONFIG_UNKNOWN | 语法、类型、未知字段 | 不发布 |
| CONFIG_LIMIT / RESOURCE_CHANGED | 解析/导入资源限制，或加载期间文件变化 | 拒绝候选，不发布 |
| CONFIG_UNSUPPORTED | 当前阶段不支持的配置或服务动作 | check 返回非零，不能假装完整成功 |
| IMPORT_CYCLE / DUPLICATE / ROUTE_CONFLICT / PROFILE_REF / SNIPPET_REF | 结构或引用错误 | 不发布，保留来源链 |
| RESOURCE_IO / TLS_INVALID / UPSTREAM_DENIED | 目录、证书或上游策略失败 | 不发布，错误脱敏 |
| RESTART_REQUIRED | 启动配置发生变化 | 整体拒绝，不部分应用 |
| UNAUTHORIZED / PROTOCOL / BUSY | 管理身份、帧或容量错误 | 不启动候选构建 |
| PREPARE_TIMEOUT / SHUTTING_DOWN | 准备过期或实例关闭 | 发布前取消，不晚到提交 |
| INTERNAL | 无法分类的内部故障 | 不泄露内部敏感数据，拒绝候选 |

客户端报文问题通过 400/404/405/421 等明确响应；上游连接失败 502、读取超时 504。外层总响应头预算耗尽时，HTTP/1.x 尝试发送 504；HTTP/2 取消超时流，不追加原始 HTTP/1 报文、不关闭整个连接，日志记录 reset_stream 而非声称已返回 504。不能把后端故障伪装为配置成功或返回其他站点内容。

## 9. 构建与检查基线

- 使用 Rust Stable，edition 2024；KDL 6.7.1 声明最低 Rust 1.95，工程声明相同下限。实际已验证工具链和完整锁定版本以验证记录为准，不声称在未运行的旧工具链通过。
- 直接使用 Pingora 0.9.0 的 openssl/proxy 功能，关闭默认可选功能；不接入 BoringSSL、rustls、ACME、负载均衡或网络缓存。
- `arc-swap` 1.9.3；Tokio 提供测试及运行时所需异步能力；async-trait 用于 Pingora 回调；直接 openssl 依赖用于安全 ex_data 类型和内存证书验证。002 按用户明确要求添加 Clap 派生 CLI，锁定版本 4.6.7，复用已有传递依赖，未增加其他依赖或测试工具。003 经用户访谈明确批准直接使用既有 socket2 0.6.5，并显式开启 Tokio 的 net/signal 功能；没有其他新增依赖或测试框架。005 经批准直接依赖 nix 0.31.3（fs）和 httpdate 1.0.3；Tokio fs 虽获批但实现未使用、未开启。Cargo.lock 仅 rill 依赖列表变化，无新包升级或测试框架。007 经用户批准直接启用既有 serde 1.0.229 derive、serde_json 1.0.154（本地缓存新锁包 zmij 1.0.23）及 nix 0.31.3 user 功能，无其他依赖升级或新工具/测试框架。010 经批准直接复用锁定的 bytes 1.12.1 及 nix 0.31.3 的 dir feature；009 经批准启用既有 nix 0.31.3 的 inotify feature，不新增 crate；完整传递依赖固定在 Cargo.lock。
- Debian 构建需要 Rust、C 工具链、make 和 Perl。Pingora 0.9.0 的 pingora-openssl 无条件启用 openssl 的 vendored 功能，因此本基线使用锁文件中的 openssl-src 构建 OpenSSL，而不是系统 openssl 命令对应的库。不要将两者版本混为一谈。未启用 OPENSSL_NO_VENDOR 覆盖；系统动态库方案须单独验证，008 部署说明必须以实际链接结果为准。001 不安装系统包、不引入测试框架或额外 dev-dependencies。
- 001 必须通过 cargo fmt --check、cargo check --locked、cargo clippy --locked --all-targets -- -D warnings、cargo test --locked。测试不监听公网、不需要真实密钥或额外证书工具。
- 003/004/005 验证真实监听、HTTP/2、优雅关闭；006 验证真实流式代理、WS/SSE；007 验证管理权限、真实 Reload 和并发原子性；008/012 补齐部署与性能门禁。不能用 001 的 TLS 字节流验证替代这些端到端验收。

## 10. 重要限制

001 解除技术机制可行性阻塞，002 提供配置语义检查，003 提供 HTTP/1.x run 和基础代理，004 提供真实 HTTPS/SNI、HTTP/2、证书检查及安全重定向。005 已准备静态 root；006 支持 HTTPS 上游和 WS/SSE。check 仍不宣称 DNS、连通性或上游 TLS 信任已验证。007 已实现 Reload；未执行独立全代码库安全审计或生产性能测试。合同如需改变通配层级、路径映射、Host/SNI 或快照代次边界，必须先更新计划并获得确认。

006 的合法 101 是最终响应头，先检查请求头总期限再进入 Streaming；SSE 在普通最终头后逐段流式传输，不聚合且无响应总时长限制。read-timeout 为上游相邻读取期限，idle-timeout 用于下游读写和连接池；不承诺单向数据刷新共享双向 idle，也不承诺通用 TCP 半关闭。响应头后故障仅中止，不追加 HTTP；保留既有 30 秒排空。

## 11. 访问日志运行边界

配置与操作见[可观测性配置与运维](configuration.md#可观测性配置与运维)。运行状态拥有一个共享日志线程；check 和候选准备不启动日志线程。请求持有开始时的快照策略，Reload 不改变进行中的请求。

访问事件 `event=access` 使用 `timestamp_ms`、可为 null 的 32 位十六进制 `request_id`、`generation`、不透明 `site`/`route`、`method`、`status`、`body_bytes`、`duration_ns`、`outcome`。方法固定 GET、HEAD、POST、PUT、DELETE、OPTIONS、PATCH、CONNECT、OTHER；outcome 为 completed、error、cancelled、timeout。status 和正文计数未知时为 null 而非 0。普通、静态、代理和 WS/SSE 在请求或会话结束时记录，而非响应头时；升级流正文计数不是 WS 应用 payload 或 TCP 线缆字节。

事件不包含原 URL、查询、Host、客户 IP、头、cookie、密钥或原始错误。site/route 身份是最小规范化站点域名和路由模式的长度前缀 SHA-256 前 16 字节，不按路由序号；知道配置的人可计算并关联，不是加密匿名化。未匹配的身份为 unknown；不再保留指标路由槽或 overflow 标签。P0 运行事件保留 `time_ms` schema 和管理员诊断字段，可能包含文件名或监听地址。

运行事件与访问事件共享最多 4096 条、每条序列化不超过 4 KiB 的队列和唯一 stderr writer。生产者在级别或访问采样后有界复制并尝试非阻塞锁和发送，满、关闭或锁竞争时丢弃；写出失败不改变业务结果。没有指标端点或导出诊断计数。访问开关独立于运行日志级别，log-level off 不关闭访问日志。尚未绑定 owner 的 check 和早期启动错误走同步路径，此时尚未接受业务请求。

关闭先停止准入并最多等待 1 秒正常 flush；超时后 worker 停止新写并清理队列，但已经阻塞的 stderr 写无法取消，不保证完整送达。采样是每千分比周期计数，不是独立随机概率。

Prometheus 指标、指标监听、OpenTelemetry SDK/OTLP 导出和 span worker 已移除；没有相关环境变量检查或 trace/span ID。代理继续剥离外来 traceparent、tracestate 和 baggage，不再生成或注入追踪上下文。历史 011/012 验收记录描述当时版本，不代表当前仍提供指标或追踪。
