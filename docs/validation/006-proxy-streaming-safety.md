# 006 完整代理与流式安全验证记录

## 状态与范围

006 实现及 79 项检查通过，最终新上下文独立只读复审 No issues found、Merge verdict OK，复审者未自行运行测试；无安全复审阻塞。此前 TLS/DNS 和 WS 接线各有只读预审 OK。独立完成提交由父代理核对后执行。005 已独立提交 `2b9fe02e4dd84b82961f17db939495fb0211d589`；007–012 未开始，本次授权不自动启动 007。

006 不新增依赖、工具或 DSL，不改锁文件。用户通过实施访谈确认 DNS 刷新失败策略及标准 OpenSSL 环境信任来源。实现采用请求触发 60 秒 DNS 应用缓存刷新，而非真实 DNS TTL；失败不返回过期地址，失败退避 1 秒。每路由只串行刷新，最多 16 个真实阻塞 resolver，许可随 blocking 任务持有；取消等待不发布迟到结果。全部候选检查去重且最多 64 个，任何禁止候选整次拒绝；单请求固定候选，最多 8 个连接建立失败尝试，不重放已建连或已发送请求。

HTTPS 使用校验后的 SocketAddr，DNS 名链和主机名校验；IP 链验证开启，非空 sni 避免框架禁验证，OpenSSL 对 IP 不发送 SNI。IP 以 hook 替换错误的 DNS 名检查，严格匹配 iPAddress SAN 字节，新建和复用连接均在 HTTP 字节前核验，失败 502、不入池、不重试。上游 URL host 决定身份，转发 Host 保留已验证客户端 authority。标准系统 OpenSSL 信任路径受管理员进程环境 SSL_CERT_FILE/SSL_CERT_DIR 影响。上游固定 H1，入站仍 H1/H2。

## 任务与验收证据

| 任务/验收 | 实际测试与结果 |
| --- | --- |
| T01 / AC01 | `tests/https/upstream.rs` 的 `dns_upstream_verifies_name_and_preserves_host` 验证 DNS 身份及原 Host；`ip_upstream_verifies_san_without_sni_on_reuse` 验证 IP SAN、无 SNI 和池复用；`invalid_upstream_certificates_never_receive_http` 验证错误 IP、DNS SAN 冒充 IP、错误 DNS、过期及未知 CA 返回 502，后端未收到 HTTP 字节。`src/proxy/dns.rs` 的 `expired_candidates_do_not_survive_denied_refresh` 验证过期旧候选被新策略拒绝后不再返回，失败后再次 Err。既有 URL、地址策略回归通过。 |
| T02 / AC02 | 既有不可信转发头、可信链及 hop 字段清理回归通过；TE/CL 规范化后强制关闭连接回归通过。 |
| T03 / AC03 | `websocket_frames_survive_request_budget_and_sigterm` 验证合法 masked 双向帧三轮，跨 50ms request 期限，SIGTERM 后继续并正常 Close、退出 0；`websocket_request_gate_never_contacts_backend` 验证非法请求不连后端；`invalid_upstream_switching_protocols_is_rejected` 拒绝坏 101 Accept、协议和扩展。`rfc_handshake_and_accept_validation` 覆盖 key/Accept 已知向量及重复、错误 Accept。`sse_chunks_stream_past_request_budget_and_drain_on_sigterm` 在首块到达 channel 后才放行后块，跨 50ms 且 SIGTERM 后完成、退出 0。 |
| T04 / AC04 | `websocket_idle_connection_closes_within_timeout` 验证有限 read-timeout 关闭；既有连接、超时和 003 HTTP 双向 2MiB 流回归通过。64MiB 静态回归仍通过，不作为代理大体资源或 WS 高并发内存基准。响应头后故障只中止，不追加 HTTP；既有 30 秒排空保留。 |

WS 仅 HTTP/1.1 GET，要求规范 16 字节 base64 key、version 13、Connection Upgrade 和无请求体；禁用 403、坏字段 400，不支持协议、CONNECT 和 H2 extended CONNECT 为 501，框架可能提前拒绝非法 H2 消息。先清理 hop 字段再恢复已验证握手头；不提议扩展，拒绝未提议扩展，子协议须为提议内单选。101 须通过 Accept/Upgrade/Connection/framing 检查，意外 101 拒绝；合法 101 也是最终头，检查总期限后进入 Streaming。SSE 最终头后逐段流式传输，不聚合、不限制总时长。read-timeout 是上游相邻读，idle 为下游读写及池闲置；不承诺单向流量刷新共享双向 idle 或通用 TCP 半关闭。

## 实际隔离门禁

父代理在与 src/tests/Cargo 相同的只读副本运行，bwrap 独立 loopback 网络、PID 和 mount，无外网；可信 `/etc/hosts` 仅有 `127.0.0.1 localhost` 与 `::1 localhost`。toolchain/registry 只读，UID 65534、GID 0，补充组清空，capability 全部移除；4 GiB scratch，单进程 AS 2 GiB、CPU 300 秒、nproc 64、nofile 256、fsize 512 MiB，墙钟 600 秒加 10 秒 kill。Rust/Cargo 1.99.1，vendored OpenSSL 3.6.3。

清空环境后白名单与 005 相同：`PATH=/opt/rust/bin:/usr/bin:/bin HOME=/scratch/home TMPDIR=/scratch/tmp CARGO_HOME=/scratch/cargo CARGO_TARGET_DIR=/scratch/target CARGO_BUILD_JOBS=2 CARGO_NET_OFFLINE=true CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS=-C link-arg=-Wl,--threads=2 LANG=C.UTF-8 LC_ALL=C.UTF-8`；测试线程 2。

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --check` | 通过 |
| `cargo check --locked --offline -q` | 通过 |
| `cargo clippy --locked --offline --all-targets -q -- -D warnings` | 通过 |
| `cargo test --locked --offline -q -- --test-threads=2 --nocapture` | 79 项通过、0 失败：6 纯核心、15 配置、7 可行性、18 HTTP、16 HTTPS、17 static。 |

过程 Clippy 首次发现 DNS 类型复杂度、公开接口的 `Result<_, ()>` 和 large enum；改用自有 `ResolutionFailed`、`Candidates`、`Box<PreparedPeer>` 并修正测试样式，未关闭 lint。

## 限制

resolver 取消、许可耗尽和多代 TLS 池隔离未逐项动态验证；取消许可/迟到发布及多身份池隔离仅源码审阅。TCP 半关闭和单向共享 idle 不作承诺；真实 WS 30 秒强制排空未单独重跑。未做 006 独立基准、WS 高并发内存测量、部署性能或 release build；后两者留后续阶段。以上不等于全部安全审计通过。
