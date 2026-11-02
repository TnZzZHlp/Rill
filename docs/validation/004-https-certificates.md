# 004 HTTPS、证书与复审记录

## 范围与交付

用户授权实施 004，复审、测试通过后独立提交；随后明确要求尽可能使用 Sub Agent。仅实施本阶段，不进入 005。不修改 writing-block.md，不增加依赖、dev-dependencies 或测试工具；测试通过既有 openssl API 生成临时 EC 证书和权限 0600 的私钥，退出清理，不提交密钥。

| 路径 | 交付 |
| --- | --- |
| src/tls/cert.rs | PreparedTls、受限 PEM 读取、稳定性检查、证书链/有效期/用途/密钥/SAN 校验、预构建上下文 |
| src/tls/resolver.rs | Pingora 异步 TLS 回调；单快照 SNI 选择及握手身份；不读取文件，无兜底证书 |
| src/config/prepare.rs、src/runtime/state.rs | check/run 共用证书准备；TLS 与路由共用一个 RuntimeSnapshot 根 |
| src/runtime/server.rs | HTTP/HTTPS 全监听绑定、IPv6-only、10 秒 TLS 握手、10 秒 H2 前言期限、HTTP/2 空闲上限及既有排空 |
| src/proxy/app.rs、handler.rs、request.rs | TLS/Host 隔离、HTTP/2 authority 和每流预算、308 重定向、真实 TLS 转发协议 |
| tests/https/、src/proxy/request.rs 测试模块 | 真实 rill 进程、OpenSSL 客户端及 Pingora HTTP/2 客户端的 12 项集成验证，以及 1 项 authority 边界回归 |

TLS 至少 1.2，ALPN 优先 h2，其次 http/1.1；关闭会话缓存、tickets 和重新协商。所有证书定义均准备，证书最多 1 MiB/16 项、私钥最多 64 KiB。SAN 不回退 CN、不接受部分标签通配或末尾根点；通配 site 须由相同通配 SAN 覆盖。链验证以管理员提供的末项为准备期锚点，不等同于所有外部客户端均信任该证书。

## 实际验证

开发过程中执行 cargo check/clippy、HTTPS 集成测试及全量测试。最终对当前源码另外执行离线隔离验证：源码、工具链、注册表只读，独立网络/PID/挂载命名空间，仅隔离 loopback；清空环境后设置明确允许值；全部写入 4 GiB 临时 scratch，退出销毁。不暴露宿主 HOME、凭据或管理 Socket，不访问外部网络。

最终沙箱使用既有 bwrap、setpriv、prlimit 和 timeout；UID 65534，GID 0 仅用于读取既有组可读注册表文件，补充组清空、capabilities 全部移除。每进程地址空间 2 GiB、CPU 300 秒、进程/线程数 64、文件描述符 256、单文件 512 MiB；整体墙钟 600 秒，超期 10 秒后强杀。Cargo 并行构建数 2、测试并行数 2，链接器线程数 2。环境白名单：PATH=/opt/rust/bin:/usr/bin:/bin，HOME=/scratch/home，TMPDIR=/scratch/tmp，CARGO_HOME=/scratch/cargo，CARGO_TARGET_DIR=/scratch/target，CARGO_BUILD_JOBS=2，CARGO_NET_OFFLINE=true，CARGO_INCREMENTAL=0，CARGO_PROFILE_DEV_DEBUG=0，RUSTFLAGS=-C link-arg=-Wl,--threads=2，LANG/LC_ALL=C.UTF-8。实际 Rust/Cargo 1.99.1，链接的 vendored OpenSSL 为 3.6.3。

| 最终沙箱内命令 | 结果 |
| --- | --- |
| cargo fmt --check | 通过 |
| cargo check --locked --offline -q | 通过 |
| cargo clippy --locked --offline --all-targets -q -- -D warnings | 通过，无警告 |
| cargo test --locked --offline -q -- --test-threads=2 | 48 项通过：1 纯函数、15 配置、7 可行性、13 HTTP、12 HTTPS；0 失败 |

沙箱搭建曾因缺少 alternatives 挂载、组读权限、默认链接器线程导致 2 GiB 地址空间不足而失败；补齐只读工具链目录、采用注册表组读并将链接器线程限制为 2 后，以上最终命令全部通过。未通过改变业务代码或放宽源码写权限绕过限制。

## 任务与验收证据

| 任务/验收 | 用例和实际结果 |
| --- | --- |
| 004-T01 / AC01 | invalid_certificates_and_keys_fail_with_located_redacted_errors：过期、未来生效、密钥不匹配、域名不覆盖、仅 CN、加密 key、损坏 PEM、多 key 和末尾根点 SAN 均退出 1；输出定位错误，不含 PEM、口令或损坏文件中的敏感探针 |
| 004-T01 / AC01 | wildcard_coverage_requires_the_entire_configured_namespace：相同通配 SAN 接受，有限个精确 SAN 及不同层级通配 SAN 拒绝；malformed_or_expired_intermediate_chains_are_not_ignored：正确链接受，乱序与已过期中间证书拒绝 |
| 004-T02 / AC02 | sni_selects_site_certificates_and_never_falls_back_for_unknown_names：共端口多证书选择正确，两个别名复用同一证书；未知/缺失 SNI、根域、多级子域拒绝握手；同证书别名的 Host/SNI 不同仍返回 421；后端收到 https 转发协议 |
| 004-T02 / AC02 | http2_carries_real_requests_and_rejects_host_coalescing_on_the_same_connection：ALPN 必须为 HTTP/2；无 Host 的合法 authority 可代理到 HTTP/1.1 后端，路径/查询不变；同一客户端连接依次返回 200、别名 421、200，未跨站合并 |
| 004-T02 / AC02 | ipv4_and_ipv6_https_listeners_can_share_a_port：127.0.0.1 与 ::1 共 HTTPS 端口均完成证书验证和真实代理 |
| 004-T02 / AC02 | http2_request_deadlines_reset_only_the_expired_stream：同一 HTTP/2 连接并发慢/快请求，慢流总期限耗尽并取消；兄弟流和随后新流仍返回 200。建立单次收到上游请求头的同步，避免把线程调度顺序误当作协议顺序；额外独立重复 3 次通过 |
| 004-T02 / AC02 | http2_missing_authority_with_host_never_reaches_upstream：真实 H2 帧携带合法 Host/SNI 但缺失 authority，在触达上游前拒绝；http2_host_without_uri_authority_returns_400 直接验证 Rill 边界返回 400，不仅依赖框架拒绝 |
| 004-T02 / AC02 | http2_incomplete_preface_closes_after_deadline：TLS/h2 协商后只发部分客户端前言，约 10 秒关闭，不永久占用接受槽；既有完整 H2 请求和兄弟流验证仍通过 |
| 004-T03 / AC03 | redirects_use_the_matched_domain_and_preserve_path_and_query：308 使用规范化匹配域名和推导的自定义 HTTPS 端口；不反射客户端端口，路径 %2f 和查询 %2F 保留；未知 Host 返回 404，无重定向 |
| 004-T04 / AC04 | check_validates_certificates_without_binding_or_contacting_a_service：HTTPS 地址已被占用仍 check 成功，不连接上游，不修改密钥；既有 check 无监听/无管理请求验证仍通过 |
| 004-T04 / AC04 | https_response_survives_graceful_shutdown_and_delivers_the_full_chain：真实客户端验证自定义根/中间链，收到第一块后 SIGTERM，继续收到后续块，进程退出 0；HTTP/2 既有连接也正常排空退出 |

## 复审状态

执行者已对照配置契约、运行时、Pingora/OpenSSL 接口及测试完成边界复审。按用户最新要求另使用 Sub Agent 完成分工：

- TLS/证书独立只读复审未发现阻塞问题；指出会话恢复拒绝、重新协商和全部链约束组合未逐项动态覆盖，不将 007 尚未实现的生产发布冒充已验证。
- HTTP/2/重定向/生命周期独立复审指出有 Host 时缺失 authority 的契约偏差；主代理还确认 H2 初始前言等待没有期限。交由 worker 修复，保持一个写入者，不修改依赖或其他阶段。
- 新上下文只读复核确认固定前言期限不可由滴入重置、Clone 共享同一预算、完成前言后解除；指出原始帧回归尚不能单独证明 Rill 返回 400，因此另由 worker 补最小纯函数回归，不扩张生产 API。
- 修复和补充回归后，主代理在上述隔离沙箱重新执行全部门禁，48 项通过；最后一项回归的独立复核已确认可关闭证据意见，最终结论 OK，无未解决问题。

子代理仅负责源码复审和约定路径的修复，没有运行项目或提交；动态检查、发现核验、验收与最终提交由主代理负责。

## 后续边界

- HTTP/2 总响应头预算到期取消该流，而不是向加密流拼接 HTTP/1.1 的 504；日志明确 reset_stream。框架自身上游读取超时仍使用网关错误处理。
- 证书只在准备期读盘；握手保留选中快照，HTTP 每请求/流另取快照。尚无 publish/Reload，实际证书热更新和跨发布生产验收归 007；001 的机制验证不代替该验收。
- HTTPS 上游、WebSocket/完整 SSE 与代理增强归 006；静态资源归 005；生产构建、部署和性能仍由后续关卡验证。
- 本记录是 004 实施验证，不宣称独立全代码库安全审计或生产压测。
