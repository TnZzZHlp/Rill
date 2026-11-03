# 008 P0 验证与交付记录

## 状态与范围

008 验收通过：修复后完整基准、原始数据核验及最终交付独立复审均已完成；阶段勾选见子计划及总计划，独立完成提交见 Git 历史。007 基线提交为 `321aed0`。本记录汇总已有真实执行结果，不表示本次文档起草重新执行了项目。范围为 P0，不覆盖 ACME、HTTP/3、完整可观测性、自动 watch 或 P1，不启动 009。

部署实测见 [008-deployment.md](008-deployment.md)；[修复后性能基线](008-performance-baseline.md)记录完整标准结果、统计口径及客户端限制。

## 原文第 12 节 T01–T18 映射

以下是行为证据而非阶段验收勾选；完整测试命令和日志见下节，具体输入与断言可追溯到所列测试源码和 002–007 验证记录。

| 项目 | 已观察行为和证据 |
| --- | --- |
| T01 | `tests/http_proxy/routing.rs::hosts_and_paths_are_deterministic_and_unknown_hosts_never_reach_a_backend`：三个域名的 `/api/exact` 均命中同一 exact 后端，通配站点命中不同 marker；两种声明顺序均断言。 |
| T02 | `exact_domains_override_single_label_wildcards`：精确域名优先，不同后端区分结果，通配不匹配多级子域。 |
| T03 | `exact_and_longest_prefix_routes_choose_different_backends`：精确、最长前缀和兜底分别命中，`/api` 不误匹配 `/api/*`。 |
| T04 | `tests/reload/transactions.rs::reload_updates_keepalive_snapshot_and_cli`、`docs/validation/007-atomic-reload.md`：真实 reload 后新请求使用新快照。 |
| T05 | `failed_reload_preserves_state_and_generation`：非法配置失败，旧状态和代次保留；部署实测另验证旧路由。 |
| T06 | `tests/https/reload.rs::tls_reload_changes_new_handshakes_and_keepalive_routes_and_rolls_back_invalid_cert`：坏 PEM、密钥不匹配失败回滚，旧 TLS 保留。 |
| T07 | 同一 TLS 测试：有效证书轮换后新握手获得新指纹。 |
| T08 | `tests/https/reload.rs::tls_websocket_survives_successful_and_failed_reload` 及 007 流测试：已有 WebSocket 跨成功与失败 reload 保持。 |
| T09 | `tests/static_files/responses.rs::spa_fallback_requires_navigation`：导航 GET 子路由返回 index 的实际 body。 |
| T10 | 同一 SPA 测试：不存在的 JS 不回退 HTML，返回 404。 |
| T11 | `tests/static_files/security.rs`：遍历、编码、magic link、链接切换和特殊文件边界；详见 [005](005-static-files.md)，不等于排除所有文件系统竞态。 |
| T12 | `tests/reload/transactions.rs::changed_imported_profile_updates_proxy_timeout`：只改导入 Profile，超时行为由 504 变 200。 |
| T13 | `tests/configuration.rs::import_cycles_and_file_aliases_are_rejected_with_both_locations`：循环被拒并保留来源；见 [002](002-configuration-check.md)。 |
| T14 | `conflicting_domains_routes_and_missing_tls_references_are_rejected`：重复和冲突定义校验失败，见 [002](002-configuration-check.md)。 |
| T15 | [006](006-proxy-streaming-safety.md) 的 `sse_chunks_stream_past_request_budget_and_drain_on_sigterm`：分块跨请求预算，SIGTERM 后完成，不以整包缓冲替代流式。 |
| T16 | `tests/static_files/responses.rs::large_file_range_crosses_block_boundary`：16 MiB 文件跨块非零 Range，精确比较 body、206 和 Content-Range。 |
| T17 | `tests/reload/transactions.rs::alternating_reloads_keep_concurrent_reader_snapshots_consistent`：六轮 A/B 切换，reader 每轮观察目标并核对 body/cache 配对，最终服务仍活。 |
| T18 | `hosts_and_paths_are_deterministic_and_unknown_hosts_never_reach_a_backend`：未知 Host 404，不触达其他后端。 |

补充证据包括转发头重建、HTTPS 信任与 SAN/Host/SNI、DNS 和资源边界，分别见 [003](003-http-routing-proxy.md)、[004](004-https-certificates.md)、[006](006-proxy-streaming-safety.md)、[007](007-atomic-reload.md)。`tests/reload/control.rs::run_rejects_unsafe_control_socket_paths_without_removing_entries` 使用真实 run，退出 1，检查精确绑定诊断、既有对象保留及不创建残留。真实跨 UID 授权与文件系统权限分层见部署报告。

## 实际质量门禁与最小修复

原始完整基准的 warm small 约 44.067 ms、178.1 RPS、CPU 约 4%，暴露 Nagle 遗漏。经用户授权，生产变更仅在 `src/runtime/server.rs` 共用 bind 中调用 `set_tcp_nodelay(true)?`；Linux accept 继承由 `bind_accepted_socket_inherits_tcp_nodelay` 的真实 accepted socket 断言验证，共用 bind 覆盖 HTTP/HTTPS 及 IPv4/IPv6。红副本仅移除该行，真实 `accepted.nodelay()` 断言失败、exit 101；绿版通过。独立源码复核无可定位问题。

最终红绿组合使用不同 `CARGO_TARGET_DIR`：`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo test --locked --offline --no-fail-fast -q -- --include-ignored --test-threads=2 --nocapture` 和 release 构建成功。111 项：core 18、config 15、feasibility 7、HTTP 18、HTTPS 19、reload 16、static 18；real DNS ignored 测试在隔离网络运行，resolver 采用 loopback UDP 53、timeout 30、attempts 2、ndots 1，reload 组 32.29 秒通过。日志：`data/008/checks/quality-release.log`、`regression-red.log`、`runtime-dependencies.log`。

OS 隔离使用只读 source/toolchain/registry，清空环境后白名单，UID 65534/GID 0、无补充组和 capability，私有 PID/mount/net namespace 仅 loopback；可信父层仅在该 netns 放开低端口以运行 UDP 53。jobs/linker/test threads 均 2，CPU affinity 6,7；`RUSTFLAGS=-C link-arg=-Wl,--threads=2`、`CARGO_INCREMENTAL=0`、`CARGO_PROFILE_DEV_DEBUG=0`、`CARGO_NET_OFFLINE=true`。质量进程 AS 2 GiB、CPU 300 秒、nofile 256、nproc 64、fsize 512 MiB；生产构建 AS 3 GiB、CPU 900 秒、fsize 1 GiB。最终组合 scratch 8 GiB、wall 1800 秒，早期单独质量门禁为 4 GiB/600 秒。

新二进制 `/var/tmp/rill-008.zWfZ2O/artifacts/rill-nodelay`，18,741,536 字节，SHA-256 `c6be369b6d7b122924b5886421daf459491020eb8ff499e158f71d29b2484ac2`；旧版 18,740,824 字节，SHA-256 `a1607fff7bccb4afdbdbb85e3416347ecdda338dfaaadd62250ceca221ccafa8`。二进制不纳 Git。

## 性能证据边界

修复前标准 full 完成 108 个 summary（HTTP 72、stream 24、idle 6、reload 6），结构化数据与原始 request/event/window/hash/resources 经独立复核对齐；有 60 条 client warning。外部 `artifacts/full` 保存 351 文件、197,916,147 字节及 manifest；仓库 `data/008/before` 保存 219 个摘要、资源、环境和完整原始清单，合计 1,366,133 字节，selection 标记校验值；逐请求事件 GZ 未纳 Git。不能把这份 full 冒充修复后结果。

修复后先通过缩短 pilot，再以同样标准完成 108 轮。`full-nodelay` 的 351 个产物共 371,583,061 字节，逐请求、逐连接事件、Reload 窗口、资源峰值及文件校验均经独立复核。HTTP small warm 三轮 p50 为 0.398/0.398/0.396 ms；72 轮有客户端限制警告，不解释为饱和容量。`data/008/after` 的 219 个所选文件共 1,364,875 字节。原始数据独立保留，未覆盖修复前结果。

## 失败与修复记录

- resolver 早期 timeout 1 误配导致 ignored 测试失败，恢复 30 后通过。
- 双核旧 DNS fixture 曾一次 CLI 45 秒退出 3；推测与 listener 关闭重试边界有关，扩大 2 秒接收窗口后多次通过，根因未确定。
- Nginx 原绝对临时目录不适合只读沙箱，改为显式 scratch；CA 缺 critical keyUsage 被 Python strict 拒绝，修正证书链而未关闭验证。
- 基准增加 warm 连接至少 95% 真实复用、WS/SSE 完整事件 3 秒进度、窗口计数、进程组清理与有界数组，独立复审关闭。
- 红绿共享 target 曾误复用红 unit 产物（栈源码行 297 而绿版 298），改用完全不同 target 干净重验；不能将此次误用解释成生产 TCP 继承失败。

父代理只读针对性复核 static_files/root、proxy/headers、control/socket/client/server、runtime/state/reload、tls/resolver 及相关动态测试；不是新的全面安全审计或渗透测试，不宣称没有漏洞。

## 原文第 13 节交付清单

| 要求 | 产物或状态 |
| --- | --- |
| 可编译运行项目、锁定依赖 | `src/`、`Cargo.toml`、`Cargo.lock`；release 实测及外部二进制见上。 |
| KDL 与 CLI | `docs/configuration.md`、`README.md`。 |
| 示例配置 | `examples/`，包括 `examples/deploy/`。 |
| systemd、Podman | `deploy/systemd/`、`deploy/podman/`；受控环境和模板差异见部署报告。 |
| 自动化测试 | `tests/` 与源码单元测试；111 项结果见质量门禁。 |
| 性能脚本 | `scripts/bench/`；修复后完整结果及独立核验见性能报告。 |
| README、架构和验收材料 | `README.md`、`docs/architecture.md`、本文、部署报告及性能报告。 |

最终独立交付复审的 Podman swap 描述问题已修正，与实际限制一致；未发现其他可定位阻塞项。本记录不承诺任意发行版、rootless 路径或饱和性能已验证，009 未开始。
