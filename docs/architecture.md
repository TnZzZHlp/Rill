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
| src/webdav/ | DAV 方法、FD namespace、临时写事务、持久属性、独占锁及进程 registry | 独立 HTTP 栈、第二个配置发布点 |
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

### Caddy 动态代理（006 已验收）

动态目标与固定上游使用独立公网地址检查；每次请求重新解析，所有候选通过后固定连接地址，不以连接池命中跳过检查。 DNS 完成后重新采集本机接口，且每次新连接或复用前再次检查固定候选与入口；采集失败拒绝。接口检查与内核建连不能原子化，之后由可信管理员更改网络配置属于受信任控制面边界。站点快照持有预备信任材料；动态与固定上游连接池身份隔离，动态同源 8 条物理连接预算跨代且包含握手与空闲池。活动请求持有进程许可至请求结束；DNS blocking 任务共享既有 16 槽，超时任务仍持槽至真实结束。检查不连接 DNS 或上游；独立验收见 [020](validation/020-caddy-dynamic-proxy.md)；010 组合验收与资源实测见 [024](validation/024-caddy-integration-release.md)，已通过最终门禁及独立复审。

### Caddy 能力补齐 002 的策略边界

链模型经现有配置管线验证，正则以 PikeVM 编译；请求持有单个快照，策略选择的固定代理和静态动作复用既有资源与安全边界。PikeVM 不使用 DFA，运行工作区独立于编译策略限额，分配前预留全进程 64 MiB 额度，匹配结束释放，不等待；不足时返回 503。工作区上界按 NFA 状态、捕获槽、分支边及栈增长计算，而非将库的 `memory_usage` 当作完整缓存容量。

### Caddy 能力补齐 003 的错误链边界

`http_policy` 负责有序动作、捕获冻结与共享预算，`proxy/policy.rs` 桥接请求快照；静态响应先准备再发送，以类型区分本地缺失/方法不支持/临时不可用与安全、容量失败，不能只凭状态码进入错误链。主链和错误链共享 1024 步及 4 次重入，错误链最多进入一次；普通上游 404 不进入。错误文件保留原状态，禁止条件请求或 Range 将其变成 304/206；错误链上游 2xx/3xx 在提交头前拒绝，503 的处理只能保持 503。已发送响应头后不追加第二份响应。

响应头声明先按主链实际执行顺序登记，进入错误链后追加错误链实际执行的声明；全部条件基于最终响应原始状态和 MIME，再按登记顺序应用。当前仅实现 chain 内声明，不引入全局/site/local 新声明层。用户已确认：动作执行时尚未确定上游则使用 `{upstream-authority}` 明确失败，不填空、不猜目标、不重放规则；延后响应过滤可在代理目标确定后读取规范连接 authority。

## 3. 单根快照与原子发布

RuntimeSnapshot 包含标准化配置、Router、准备好的代理或静态路由、PreparedTls 和日志策略。以下为快照内容：

- 单调递增的 `Generation`；溢出时失败而不是回绕。
- 站点及 Host/路径路由表。
- 上游端点、超时、转发头和访问控制策略。
- 静态 root 句柄及静态响应策略。
- 同代次的 SNI 证书集合、私钥及验证结果。

`RuntimeState` 拥有唯一 `ArcSwap<RuntimeSnapshot>`。准备时不写活动状态，管理提交串行化：读取配置、准备全部资源、比较启动设置、赋代次、一次 store，随后返回成功。关闭和提交共用短暂的 std Mutex gate，store 前检查关闭状态和 30 秒期限；blocking closure 只构建，不能访问 state 或 store。代次 checked_add 防溢出。启动快照使用后释放，不永久持有第一代；运行期 App 日志读取当前快照。失败没有发布，因此所谓回滚是保留旧快照，而不是先局部更新再反向恢复。

旧式路由 Snippet 在标准化时展开；规则链 `chain-snippet` 由 `config/parser/chain_snippets.rs` 在链解析前按引用位置展开，再复用同一规则解析、策略编译和鉴权检查。定义和引用来源有界去重保留，相对资源路径以定义文件为准；未引用定义也检查引用图和结构。规则链片段不进入运行时，不新增活动状态或配置构建路径。引用深度、复制规则/路由、步骤及逻辑 payload 使用有界预算，具体限制见配置文档。

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
- HTTPS 连接只使用已校验 SocketAddr，信任标准系统 OpenSSL 路径，管理员进程环境的 SSL_CERT_FILE/SSL_CERT_DIR 可影响信任。DNS 名使用框架链及主机名校验，不设置替代 CN。IP 启用链验证并设置非空 sni 以避免框架禁验证，OpenSSL 对 IP 不发送 SNI；仅替换框架的错误 DNS 名检查，以 hook 严格匹配 iPAddress SAN 字节，connected_to_upstream 在新建及复用连接发送任何 HTTP 字节前核验具体 IP，缺失或不符返回 502，不入池、不重试。旧语法上游使用 HTTP/1；chain 固定代理已接入 h2c 和 TLS h2，完整 gRPC 与跨协议池隔离的独立验收见 [019](validation/019-caddy-http2-grpc.md)。 CA 预算覆盖 Rill 持有的原始材料、可计量副本及元数据，不覆盖 OpenSSL 内部解析、证书对象或惰性缓存，非 RSS 硬上限；store 以候选共享并随连接租约持有。入站仍支持 HTTP/1 和 HTTP/2。
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

### 正文变换的所有权边界（能力补齐 007）

`body_transform` 拥有严格 MIME 选择、流式匹配/codec、共享预算及输出生命周期；`http_policy` 只在实际到达动作时登记并冻结实例，`proxy`/`static_files` 负责原 metadata、源表示和背压交接。候选先编译完整正则，运行时使用明确 PikeVM Cache 与既有 workspace 配额；不重新读取活动快照。

每请求 1 MiB、进程 64 MiB 只约束 Rill 自有及额外持留的变换 allocation capacity。预留后才分配；所有无界 stage 共享一次连续 arena，未用区域也全额收费，不逐块重分配整段 pending。region 紧凑仅复用同一 allocation，capture/cursor 保留相对逻辑 offset，pull 片段先复制到带券 stage 块、结束借用后才推进下级；上级全部匹配片段完成后才释放其 region 和传播 EOF。工作信用与实物同券分拆、不双计：普通输入 64 KiB、decoded/stage 各 16 KiB、最低输出 3×16 KiB，另扣 bounded pending/KMP 表、额外源持留及更多历史 owner。arena 可用量是这些存活费用之后的余量，不保证 pending 或每条规则独享 1 MiB。输入超信用明确失败；EOS 实际 drop source/decoder/decoded 和未用输入信用后才做 EOF 级联。此共享口径于 007 独立复审后获用户确认，不追认此前逐块双 allocation 的约半 MiB 过早失败。输出 `Bytes::from_owner` 持券及快照直到最后 clone 真释放，不按 write 返回或 payload 长度归零放券。64 响应许可覆盖真实 blocking worker、codec 及取消后仍在运行的计算。活动执行安全点累计两秒，输入/输出 channel 等待暂停计时；执行等待两秒到期取消交接，不发布迟到结果，不能抢占单次库调用。

原框架接收/HTTP 解析/flow-control、framing、HPACK、TLS 和内核缓冲不在此预算内。proxy 在同步 push 内预留并复制当前源块、立即 drop 框架 Bytes，之后才提交，不跨 await 持未知底层。respond 直接从候选不可变文本复制一次；变换文件读取先持预算 Buffer；listing/cache 从既有 005 有界 producer 交接时额外按源实际容量持券，源/input 重叠同时计费。codec 内部上下文与工作区也不计字节额度（固定 gzip 32 KiB/zstd 1 MiB window，decoder max log20），因此不是 HTTP 整体或进程 RSS 硬限。

同版 Pingora proxy 增加默认关闭的异步 push/next drain hook，不改 HttpTask 或 UpgradedBody；一次提交一个输入，逐输出 await 写入再继续拉取，默认同步过滤/延时路径不变。下游模块未启压缩，不新增深拷贝。h2 0.4.20 `codec/framed_write.rs:240–258` 会把小 DATA/前缀复制到既有 encoder.buf（16 KiB 默认初始容量，chain threshold 256/1024），属于上述既有 framing 存储；不将其增长或 HPACK 宣称为 007 硬限。其 `Next::Data`/`last_data_frame` 仍持原 owner 时，Rill allocation 继续计费。完整证明范围与实测见 [021](validation/021-caddy-body-transforms.md)。

### 可写 DAV 的所有权边界（能力补齐 008）

请求级真实目录项 Proof、单请求共享遍历 ledger、持 FD 的退休 lease 及跨重启私有 inode 标记已通过独立复审、非 root 完整门禁和隔离验收。普通 FS 回归不冒称实际 casefold 专用环境或每个崩溃窗口均已亲测，见 [022](validation/022-caddy-webdav.md)。

`webdav` 持有方法/条件/XML/属性/锁业务，`config` 只解析冻结终止 DSL，`http_policy` 复用鉴权 dominance 与同请求成功动作证明，`proxy` 只桥接 Pingora Session 和现有请求预算；GET/HEAD 继续使用既有静态表示/007 变换。请求固定一个快照，源 URI/Destination 的纯解释不能重新读取活动代次、重验密码、重放外部副作用或猜路径逆映射。源 URI 单次解码后以规范 unreserved 拼写复核授权域，避免百分号/重复斜线切换弱 auth；随后所有源/目标/tagged If、GET handoff、PUT parent/commit 统一经显式 NamespaceSession。session 持 StorageHandle 与 owned namespace guard、共享 Proof，不使用 ambient/thread-local 或全局路径缓存。每组件根据受约束父 FD 的真实 dentry bytes 精确匹配并复核 dev/ino/type/mount，以 NO_XDEV 拒绝内部目录及普通文件 bind mount 的跨挂载别名（不改普通 static），无精确项的折叠别名拒绝。rustix RawDir 用固定 8 KiB 栈缓冲与 cookie 增量枚举，正证可提前返回，负证必须读完；children/walk 消费相同缓存，所有实际非 dot 项和新绑定共用 4096 ledger（根占一项），私有扫描不免费，source+overwrite 旧 target 两树预检后才删目标。内部变更精确更新父缓存，释放保护后不复用。目标/子资源解释扣同请求剩余 1024 步，不按每资源重置；只有首规则已证明闭合的普通无条件/prefix 域、同 prepared handle 和明确 strip-prefix 才复用证明。复杂链可在 4096 项之前耗尽步骤，不能为保遍历成功放大工作预算。

进程 registry 以 storage-id 与 prepared root dev/ino 持锁和 namespace gate；相同身份跨代复用，移除后身份历史仍受预算约束保存，换 inode 重加要求重启。不同 ID 的同根/父子物理根（含历史）拒绝；同设备不同 mount 无法证明分离时也拒绝。DAV 路径组件拒绝 symlink、普通文件多硬链接，不改变一般静态安全 symlink 政策。namespace 的 Tokio owned gate 由实际 blocking 任务持有，覆盖父 FD 解析/复验、上传及内部修改；逻辑 token/条件在同临界区核验，不拿全局 std Mutex 跨网络等待。

check/prepare 不写 registry 或属性目录。公开低层 prepare(identity, Arc<File>)/activate(self) 保留冻结签名，配置 adapter 保真实来源和信用；Root 的 from_prepared_fd 复用原安全探测，不经 proc 路径授权，candidate 绑定原 registry 内层 Arc owner。完整候选和 watch 等资源准备完后才 activate；进程激活许可从 activation 保留到最终 gate、唯一 store、不可失败 commit。新目录/marker 由候选 RAII 拥有，失败只按身份清理自己未发布项，旧属性/锁/FD 不清空。公开 RuntimeState::new 保持成功即用语义；server 共用 new_deferred 的内部构建，startup guard 跨运行时/watch/signal/listener/resolver/control 准备，在任何处理 worker spawn 前才不可失败 commit，早期失败撤回本次新目录，保既有属性。初始内部 ArcSwap 不等于对外服务已发布。`.rill-dav` 与 `.rill-dav-put-` 是用户明确批准的内部保留名；目录 0700/文件 0600 不是同 UID 静态读屏障。旧静态 Root 在交付 FD/缓存查询前也检查词法保留名、已验证 procfd 的最终路径保留名和私有 dev/ino；procfd 路径仅做 deny，不授权。用户合并表单 d4fd7a63 批准 rustix 1.1.4（default-features=false，fs/std）及持久 inode tag/O_TMPFILE 方案。内部 owner/属性/temp 在同 parent 匿名创建，link 前已预留清理 parent FD/name/identity/信用，由 NamedFile RAII 持有至成功移交；新私有目录在 fallible 登记前也有清理 guard，quota/FD clone 失败不留本候选孤项。精确 inode 清理使用 fstatat 而不再申请新 FD，不声称能消除外部 unlink 失败。固定 user.rill.webdav.private 标记与 sync 成功后才用已验证 procfd 发布保留名并复核身份；无能力则 activate fail closed，不做先命名再打标 fallback。普通 static 文件安全重开后以固定小栈 buffer fgetxattr，标记存在或 ERANGE 拒绝；NODATA/明确 FS NOTSUP 为未标，其余错误拒绝。目录通过 O_PATH 逐祖先查固定 .rill-dav 子目录身份屏障，不枚举普通目录、不无理由读开 execute-only 目录。旧未标记格式拒绝接管。活动私有记录退休时持真实 FD/64 MiB 券与未释放临时磁盘额度，直到 nlink0 的有界回收，不能只保裸 dev/ino；Drop 仅原子退引用，不取 visibility 锁或在 Tokio 做阻塞 IO。退休标记不清，所以即使重启仅 static 配置，普通名硬链接仍拒绝。

PUT 逐块最多 64 KiB，异步 Upload 在读取和短 blocking 写任务间交接唯一所有权，网络等待不占阻塞线程；真实 IO/异步调度的清理任务持 FD、namespace、16 槽之一及磁盘额度至结束，Drop 不在 Tokio 线程同步 unlink；临时空间分配前计量，每文件 256 MiB/进程 1 GiB、300 秒。XML 1 MiB/深 32、属性 64 项/64 KiB、遍历 4096 项/深 16、锁 1024/storage 与 4096/process/3600 秒。PROPFIND Selection 保原 XML owner 并借用 Name，不克隆扩展名称；输出转义逐段写入带券 64 KiB 块，live/属性中间 owner 预留信用，全部响应构造成功后才发送 207，额度不足头前 503，移交输出块不再复制。历史、私有 inode、锁 owner、XML/遍历/存活输出信用复用既有 64 MiB policy pool，过期、删除和真正释放输出时回收；这是 Rill 的有界资源信用，不是 RSS 或文件系统总容量承诺。

完写/sync、父身份及条件重核均在最终 commit 准入 CAS 前；CAS **不等于** rename 已经在文件系统线性化。取消先赢则不 rename、精确清 temp。有效期限内 commit 准入先赢后执行无 await/无排队的不可抢占 rename、父 sync、清私有标记/file sync，可能跨过断开/期限，客户端结果未知；不阻塞 Tokio Drop 等门闩、不自动重试。rename 失败清 temp；成功后父 sync，再清私有标记/file sync，清标记或 sync 失败/崩溃可留下已提交但不可读的新 target，只记录结果未知，不删除 target 伪称回滚、不自动重试。管理员核实完整性后显式移除固定 xattr/fsync 恢复；无自动恢复 CLI。属性单资源原子替换具有相同持久化未知边界；多文件 COPY/MOVE/DELETE 及文件+属性没有整体事务承诺，207 的失败逐项保留。外部 UID/root/管理员/mount 仍属受信控制面。

独立复审、真实客户端/磁盘错误、非 root 全套、历史失败更正与已验证边界见 [022](validation/022-caddy-webdav.md)，008 已通过实际验收；不宣称全部文件系统或崩溃窗口均已实测。

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

站点文件日志使用独立有界队列和后台 worker；生产者先取得站点和全进程记录信用，再有界编码及非阻塞入队。每站点最多 1024 条、进程最多 16384 条，每条最多 16 KiB；站点、sink、worker 各最多 64，FD 最多 256，包含候选和旧代。写入前以记录所属快照的轮转策略检查大小；目录、活动文件和轮转项按文件身份约束打开与复验，失败候选只撤回自身创建项。旧请求持有其快照和 sink，单次 store 后才唤醒新 worker；关闭路径的用户态等待共用 5 秒预算，其中文件 worker 最多等待约 4.75 秒，预留最多约 250 ms 供 stderr 超时诊断和确认；stderr 阻塞时也不延长该预算。用户已明确确认五秒限制用户态关闭等待；内核不可中断 D 态 IO 可能延迟整个进程回收，不保证进程五秒退出。阻塞中的 worker 保留资源信用直到实际退出。慢盘、满队列或写失败不阻塞业务请求，诊断脱敏限频。已发布目标即使退役仍在进程内保留有界磁盘信用，每目标预留 6 文件/60 MiB，进程终生最多 64 个不同目标；这不是跨重启的文件系统总配额。按记录策略及 worker 空闲周期（最多约 60 秒加文件系统执行时间）清理超龄轮转项；短写失败先核对身份和长度再恢复原长度，无法确认或恢复时隔离后续追加并有界重试。

关闭先停止准入并最多等待 1 秒正常 flush；超时后 worker 停止新写并清理队列，但已经阻塞的 stderr 写无法取消，不保证完整送达。采样是每千分比周期计数，不是独立随机概率。

Prometheus 指标、指标监听、OpenTelemetry SDK/OTLP 导出和 span worker 已移除；没有相关环境变量检查或 trace/span ID。代理继续剥离外来 traceparent、tracestate 和 baggage，不再生成或注入追踪上下文。历史 011/012 验收记录描述当时版本，不代表当前仍提供指标或追踪。

### 鉴权准备与执行

Basic 哈希与静态令牌在候选准备阶段校验，watch 复用现有依赖跟踪，凭据随快照保留并纳入策略配额。令牌通过非阻塞打开的文件描述符检查类型、权限、大小及读取前后身份，失败候选不发布；快照保存 SHA-256 摘要，请求摘要以固定 32 字节比较。处理时间仍可随请求输入长度及其他 HTTP 操作变化，不宣称端到端恒定时间。

执行器以规则、分支与动作游标暂停并恢复，不重放已执行的匹配或动作；执行段释放静态 IO 槽后再等待鉴权。密码验证在获取进程四槽许可后进入 blocking 任务，超时或取消不会提前释放仍运行任务的许可。成功身份只在同一请求与固定快照中按动作身份复用；错误凭据及容量错误不进入错误链。未知用户执行固定成本 dummy 验证以避免立即返回，但不同算法/成本仍有可测时序差异。
