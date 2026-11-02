# 003 HTTP 路由、基础代理与复审记录

## 范围与依赖批准

用户授权实施 003，完成后复审、测试，通过后独立提交；不授权自动进入 004。本阶段提供 HTTP/1.x 客户端到 HTTP 上游的可运行纵向切片，不宣称完成 HTTPS、HTTP/2、静态文件、WebSocket 或生产 Reload。

用户通过访谈 b8ca8645-4eb4-443f-a26e-571a13202582 明确允许直接使用锁文件已有的 socket2。实际版本为 0.6.5，Cargo.lock 只增加 rill 对它的直接依赖关系；原有依赖没有升级。Tokio 显式启用既有 net/signal 功能，未增加其他依赖、dev-dependencies 或测试框架。

Pingora 0.9.0 的 ListenerEndpoint 构建及 Listeners::build 是私有接口，默认高层 Service 绑定失败时使用 expect。因此实现通过安全 socket2 API 绑定和设置 IPv6-only，以 Tokio 管理接受/信号/排空，仍使用 Pingora Listener、HTTP 应用适配、请求解析、流式代理及连接池；没有调用私有接口、改第三方源码、添加 unsafe 或另写 HTTP 协议栈。

## 交付物与职责

| 路径 | 实际交付 |
| --- | --- |
| src/cli/run.rs、src/cli/mod.rs | Clap run --config，默认 /etc/rill/config.kdl；入口只编排 |
| src/runtime/state.rs | 完整准备 HTTP 上游地址与策略；一个 ArcSwap 根持有不可变初始配置，代次为 1；未开放发布/Reload |
| src/runtime/server.rs | 全部地址成功绑定后才启动接受；IPv4/IPv6、信号、连接数量上限、停止接受、在途请求排空与总截止时间 |
| src/runtime/stream.rs | 每连接的安全可共享流，仅在单次 poll 内持有该连接的锁，用于取消超时请求后发送有限错误响应；没有全局代理热路径互斥锁 |
| src/routing/mod.rs | 精确 Host 与一级通配符索引、精确路径与按长度排序的前缀；不受声明顺序影响 |
| src/proxy/upstream.rs | 准备时解析和检查全部 DNS 候选，只按批准的具体地址连接；Pingora HttpPeer 的有限连接/读取/写入/池空闲超时 |
| src/proxy/request.rs、headers.rs | Host/路径边界、可信代理链、Forwarded/X-Forwarded-* 重建、Hop-by-Hop 清理；请求目标不重写 |
| src/proxy/handler.rs、app.rs | 每请求固定快照、动作分发、请求头总截止时间、错误码、关闭决定保留、流式代理适配 |
| src/observability/logging.rs | 结构化 JSON 行日志、级别过滤、启动/停止/故障；不记录原始 URI、凭据、请求头或完整后端错误 |
| tests/http_proxy.rs、tests/http_proxy/ | 真实 rill 子进程与本地真实 Socket 的 13 项服务集成验证，无 TLS/HTTP mock |

config 仅增加可复用的域名规范化导出及 Route.action_origin，后者确保启动准备错误定位到实际 proxy/root 节点；既有配置语义不变。tests/configuration.rs 的 run 参数错误用例改为 run --unknown，以适配 run 已实现。

## 实际检查与结果

| 命令 | 实际结果 |
| --- | --- |
| cargo fmt --check | 通过，无格式差异 |
| cargo check --locked | 通过 |
| cargo clippy --locked --all-targets -- -D warnings | 通过，无警告 |
| cargo build --locked | 开发构建通过，不冒充发布构建验收 |
| cargo test --locked --test http_proxy -- --test-threads=1 | 首轮 11 项服务验证通过 |
| cargo test --locked --test http_proxy | 加入总截止时间与分帧关闭回归后 13 项通过；并行执行通过 |
| cargo test --locked | 15 项配置、7 项机制、13 项 HTTP，共 35 项通过、0 失败 |
| git diff --check | 通过 |

实际 Rust/Cargo 为 1.99.1，环境仍为 Debian 13.7；未声称最低工具链、生产性能或完整部署验收通过。

## 任务与验收映射

| 任务/验收 | 真实证据与观察 |
| --- | --- |
| 003-T01 / AC01 | ipv6_listener_and_ipv6_upstream_are_available：IPv6 客户端和 IPv6 后端返回预期内容；其他服务用例使用 IPv4。keepalive_reuses_downstream_and_upstream_connections：同一客户端连接发两次请求，后端仅接受一个连接且处理两次 |
| 003-T01 / AC01 | graceful_shutdown_stops_accepting_and_preserves_an_inflight_response：收到第一块后 SIGTERM，确认端口停止接受而进程仍在排空，再释放后端第二块，原客户端继续接收，进程退出 0 |
| 003-T02 / AC02 | hosts_and_paths_are_deterministic_and_unknown_hosts_never_reach_a_backend：一个 site 的三个域名、大小写/末尾点/端口匹配正确；反转站点与路由声明仍正确；未知 Host、根域和多级子域返回 404，缺失/重复/非法 Host 返回 400。覆盖 T01、T18 |
| 003-T02 / AC02 | exact_domains_override_single_label_wildcards：不同后端证明精确域名优先，多级子域不命中一级通配符，覆盖 T02；exact_and_longest_prefix_routes_choose_different_backends：不同后端证明精确、最长前缀和兜底，/api 不误命中 /api/*，覆盖 T03 |
| 003-T03 / AC03 | forwarded_and_hop_headers_are_rebuilt_without_changing_the_raw_target：原始 %2f 和查询串保留；不可信 Forwarded/X-Forwarded-*、Connection 指定字段与响应 Hop-by-Hop 字段不透传；Host 保留已验证 authority |
| 003-T03 / AC03 | trusted_proxy_chain_is_validated_and_regenerated：从真实可信直接对端向左追溯，去除首个不可信跳点之前的伪造链，重建正确地址和协议；畸形可信链返回 400 |
| 003-T03 / AC03 | connection_failures_and_header_deadlines_return_gateway_errors：拒绝连接返回 502、超出 request-timeout 返回 504；response_header_deadline_does_not_reset_on_each_upstream_byte：后端持续滴入头部且每次间隔小于 read-timeout，仍在总期限返回 504 |
| 003-T03 / AC03 | request_and_response_bodies_stream_before_their_producers_finish：2 MiB 请求仅先发 4 KiB 就要求后端实际收到；2 MiB 响应先送达第一块再允许后端继续；没有整包聚合 |
| 003-T04 / AC04 | invalid_or_unavailable_startup_configuration_never_leaves_a_running_service：非法 URL、已占用监听地址、尚不支持的 HTTPS 上游均在 started 之前退出 1；不会部分启动或假装支持 |
| 003-T04 / AC04 | conflicting_framing_preserves_pingoras_forced_connection_close：CL+TE 由 Pingora 规范化为仅 chunked，保留强制关闭决定；客户端收到 Connection: close 和 EOF，不重启该连接 Keep-Alive |

## 额外真实进程检查

以下均使用受控临时配置、本地后端和真实 rill run，未请求外部不受控目标：

- 同一端口同时监听 0.0.0.0 和 [::]，IPv4/IPv6 均可访问，IPv6-only 避免双栈重复绑定。
- 复用连接上的下一请求仅滴入部分头，idle-timeout 为 100ms 时实际返回 408；相邻读取有进展不延长总头部期限。
- 本地监听积压队列填满后连接上游，connect-timeout=50ms 独立触发 504，实测约 0.072s；read-timeout=50ms、request-timeout=2s 时独立触发 504，实测约 0.071s。
- 冲突 Content-Length 返回 400；Connection 指定 Host 返回 400；超过 64 KiB 的已解析请求头返回 431；未实现的 Upgrade 明确返回 501，而不是静默降为普通代理。
- 后端一直不结束响应时发送 SIGTERM，30 秒排空总期限后实测 30.05s 退出 1。正常排空退出 0；超时有 shutdown_deadline 事件，不无限等待。
- HTTP 上游使用 localhost，启动完成地址解析，连接失败可在已验证地址之间做最多 8 次建立连接尝试；不对已建立连接上的请求失败自动重放。
- 64 MiB 上传加 64 MiB 响应完整传输成功。该开发构建进程 RSS 起点 20604 KiB，结束 RSS 25832 KiB，峰值 26048 KiB，峰值增长 5444 KiB；这是单环境观察，不是跨环境性能承诺或正式基准。
- 捕获的日志逐行可解析为 JSON，不包含探测 URI 中的 secret-marker。run 的启动配置错误也以结构化诊断输出；check 仍保留定位文本。

## 完成后复审及修正

执行者重新阅读运行时、流适配、路由、头部策略、代理回调及测试源码，对照 003-T01–T04 和 001 契约；没有委托子代理，不声称独立安全审计。

1. 核对 Pingora 源码发现 ListenerEndpoint 并非公开 API，避免把不可编译方案当作可交付。经用户批准直接使用已有 socket2，绑定错误以 Result 返回，IPv6-only 可明确设置。
2. Pingora 公开的读取超时是相邻读取期限，不能代表 request-timeout 总期限。因此以独立 watch 状态区分入口头部、等待最终响应头、响应流阶段；慢速滴入不能延长总期限，最终响应头后取消总时长限制。
3. 复审分帧时确认 Pingora 对同时存在的 CL/TE 会删除 CL 并关闭 Keep-Alive，不应臆称一定拒绝 400。初版应用重新设置 Keep-Alive 可能破坏该安全边界，已改为只调整仍允许复用的连接。
4. 对上述关闭修复执行了负向实验：临时去掉保留关闭判断，同一个最小回归在 Connection: close 断言处失败；恢复判断后通过，随后全量 35 项再次通过。没有通过修改第三方行为来掩盖问题。
5. 保留未发送请求前的地址候选尝试，禁止 error_while_proxy 自动重试，避免非幂等请求重放。全部 DNS 地址在准备时经过与字面量相同的访问策略，连接使用已批准的具体地址，没有校验后再次按域名解析的竞态。
6. 流适配只在当前连接单次 poll 内短暂持锁，不跨 await，不使用全局请求锁；请求体/响应体由 Pingora 双向背压转发。上游任务队列源码为每方向 4 项、常规 body 块 64 KiB；请求头另有应用 64 KiB 语义上限和框架约 1 MiB 解析硬边界。
7. 请求失败在最终响应头写出前才尝试错误响应；响应已开始后关闭而不追加另一份 HTTP 错误。预算到期先取消原请求再通过同一安全流发送有限错误响应，避免请求继续在后台运行。
8. 不安装会打印原始请求头的框架日志器；应用错误日志仅包含静态错误类型及配置代次。抑制代理默认详细错误/重试告警，原始 URI、头和凭据不进入日志。

## 当前限制与后续交接

- run 仅服务明文 HTTP/1.x；TLS、HTTP/2 归 004。HTTP/2 字节流不会被当作已支持功能。
- HTTPS 上游配置可做阶段性语义 check，但 run 明确返回 CONFIG_UNSUPPORTED；其实现归 006。当前阶段性 check 不声称解析 DNS 或验证连通性。
- WebSocket 允许时暂返回 501，显式 websocket #false 返回 403，006 再接入 Upgrade；不伪装成功。
- 静态 root 和 TLS 材料仍由后续资源准备器提供，当前拒绝运行这些配置。
- 地址在启动准备时解析并固定，更新需重启，自动重解析策略归 006，配置原子 Reload 归 007。系统解析器的准备时间不冒充为连接超时。
- 全局最多 1024 个接受容量槽位，待接受槽位也占配额；内核 backlog 每监听为 1024。SIGTERM/SIGINT 均执行最多 30 秒排空；初始请求头总期限为 60 秒，复用请求按路由 idle-timeout 计时。值为本阶段明确基线，不擅自新增未经配置契约确认的字段。
- RuntimeState 当前只有初始快照和读取接口，没有发布 API 或管理 Socket；不声称已完成 T04–T08、T17。

本阶段任务/验收均据实际结果更新。独立完成提交成功后关闭 003，提交失败仍待提交，不自动进入 004。G02 仍需 HTTPS/HTTP2 和完整代理阶段，不提前勾选总目标。
