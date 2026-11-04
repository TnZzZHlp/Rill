# 012 发布验收索引

本文是 T01–T18、P0 补充、组合验证及门禁的证据索引。012 全部任务、AC01–AC04 和 G07 已由父流程验收；最终独立交付复审 `633d7505-cb5e-42d3-9380-b307aeb64425` 为 FinalDeliveryMergeOK。独立完成提交以包含本记录的 Git 提交为准。独立复审 `2a36820e-8342-433b-9ba1-7272746e58ca` 为 T01 Acceptance OK，无阻断 H/M；父核验 systemd 的 54 个文件/68,809 bytes 及 Podman 的 206 个文件/426,349 bytes 大小和 SHA-256 均一致。性能后续由 `012-performance.md` 单独记录，部署后续由 `012-deployment.md` 单独记录，二者当前不以本文代替。关键结果、来源及 SHA-256 已保存在 `docs/validation/data/012/retained-files.json`；完整原始日志和测量数据仍在外部目录，可能被清理。

## 基线与证据解释

以下 `A` 指 `/var/tmp/rill-012.zsDBix/artifacts`。源码基线 HEAD 为 `a1bf0314682b9d7e1e6a7f89d0d57b2e2e72755a`，Rust/Cargo 源码相对此基线未改。只读追溯报告位于 `/root/.pi/agent/sessions/--root-github-Rill--/subagent-artifacts/2292ce3f-27a8-4f17-8700-77dc17a2ca52_worker_output.md`；它当时的“012 未执行”是该次调查的时间点结论，不是本次结果。

本次 native 质量运行的 `A/012-baseline-quality.log` 记录 176 项通过、0 失败、0 ignored，七组为 43/27/7/22/21/29/27，不是继承 011 的旧结果。`/var/tmp/rill-012.zsDBix/run-quality.sh` 记录实际隔离环境和命令：依次 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo test --locked --offline --no-fail-fast -q -- --include-ignored --test-threads=2 --nocapture`。日志是 quiet 汇总，没有逐测试名称；下表是源码测试与该全套实际运行的关联，而非逐项命名输出。IPv6 测试直接 bind，失败将使测试非零，并非静默跳过。包含 ignored DNS 用例的通过只适用于本次隔离解析器环境。

008 版本 `2059f15` 与当前 `a1bf031` 的 fresh release 构建各退出 0，见 `A/012-build-008.log`、`A/012-build-current.log` 及各自 manifest；两者 `--version` 均为 `rill 0.1.0`，版本字符串不能替代源码 SHA。

## T01–T18 对照

表中实际判定均指上述本次完整测试汇总通过，源码用例提供断言范围；独立组合证据另见下节。

| 需求 | 相关源码测试 | 本次实际及判定 |
|---|---|---|
| T01–T03、T18 | `tests/http_proxy/routing.rs` 的 `hosts_and_paths_are_deterministic_and_unknown_hosts_never_reach_a_backend`、`exact_domains_override_single_label_wildcards`、`exact_and_longest_prefix_routes_choose_different_backends` | native 全套通过；真实 Host、域名与路径路由覆盖 |
| T04–T05 | `tests/reload/transactions.rs` 的 `reload_updates_keepalive_snapshot_and_cli`、`failed_reload_preserves_state_and_generation` | native 全套通过；更新及失败回滚覆盖 |
| T06–T07 | `tests/https/reload.rs::tls_reload_changes_new_handshakes_and_keepalive_routes_and_rolls_back_invalid_cert` | native 全套通过；证书轮换与回滚覆盖 |
| T08 | `tests/https/reload.rs::tls_websocket_survives_successful_and_failed_reload`、`tests/reload/streams.rs::websocket_survives_proxy_reload` | native 全套通过；WS reload 覆盖 |
| T09–T10 | `tests/static_files/responses.rs::spa_fallback_requires_navigation` | native 全套通过；SPA 和 404 覆盖 |
| T11 | `tests/static_files/security.rs::decoding_and_confined_symlinks`、`changing_symlink_never_escapes_root` | native 全套通过；具体路径及链接边界，不是穷尽所有竞态 |
| T12 | `tests/reload/transactions.rs::changed_imported_profile_updates_proxy_timeout`、`tests/reload/watch.rs::watch_imported_profile_changes_real_timeout` | native 全套通过；手动与 watch 分别覆盖 |
| T13–T14 | `tests/configuration.rs::import_cycles_and_file_aliases_are_rejected_with_both_locations`、`conflicting_domains_routes_and_missing_tls_references_are_rejected` | native 全套通过；配置拒绝覆盖 |
| T15 | `tests/http_proxy/long_connections.rs::sse_chunks_stream_past_request_budget_and_drain_on_sigterm`、`tests/reload/streams.rs::sse_chunks_survive_proxy_reload` | native 全套通过；SSE 流、重载与排空分别覆盖 |
| T16 | `tests/static_files/responses.rs::large_file_range_crosses_block_boundary` | native 全套通过；206、范围和正文覆盖 |
| T17 | `tests/reload/transactions.rs::alternating_reloads_keep_concurrent_reader_snapshots_consistent`、`concurrent_reload_generations_are_contiguous_and_snapshots_complete` | native 全套通过；并发快照覆盖，非 coordinator 必然重叠证明 |

## P0 补充

均关联本次 native 全套日志，除特别说明外并非单项命名日志。

| 输入或边界 | 源码测试及实际判定 |
|---|---|
| IPv4/IPv6、H2 | `tests/http_proxy/lifecycle.rs::ipv6_listener_and_ipv6_upstream_are_available`；`tests/https/service.rs::ipv4_and_ipv6_https_listeners_can_share_a_port`、`http2_carries_real_requests_and_rejects_host_coalescing_on_the_same_connection`；本次全套通过 |
| 上游 TLS | `tests/https/upstream.rs::dns_upstream_verifies_name_and_preserves_host`、`ip_upstream_verifies_san_without_sni_on_reuse`、`invalid_upstream_certificates_never_receive_http`；本次全套通过 |
| 头、framing、流 | `tests/http_proxy/forwarding.rs::forwarded_and_hop_headers_are_rebuilt_without_changing_the_raw_target`、`conflicting_framing_preserves_pingoras_forced_connection_close`、`request_and_response_bodies_stream_before_their_producers_finish`；本次全套通过，不是全面走私审计 |
| DNS ignored | 全套含 ignored；仅本次隔离 DNS fixture 的实际结果，非通用解析器保证 |
| 控制和路径 | `tests/reload/control.rs::socket_bind_rejects_existing_entries_and_unsafe_parents`、`run_rejects_unsafe_control_socket_paths_without_removing_entries`；跨 UID 另由下述本次 systemd 结果支持，不借用 008 历史结果 |
| 重启项 | `tests/reload/transactions.rs::cache_budget_changes_require_restart`、`observability_policy_reloads_but_endpoint_requires_restart`，以及 `src/runtime/state.rs` 与 `tests/reload/watch.rs` 的启动项测试；本次全套通过，组合拒绝见下 |
| 排空 | `tests/http_proxy/lifecycle.rs::graceful_shutdown_stops_accepting_and_preserves_an_inflight_response`、`tests/static_files/lifecycle.rs::large_file_streams_bounded_and_drains_on_sigterm` 和上述 SSE 测试；本次全套通过 |

## 组合实际记录

| 独立 scope 与操作 | 实际结果及界限 | 证据 |
|---|---|---|
| static 首次运行：release 原配置及私有控制副本 check，启用 watch、8 MiB cache、预压缩，观察三个域名的 TLS、正文及真实 gzip/br 比较；坏语法、过期证书、有效证书 B、cache 和 metrics 地址重启项 | check 均退出 0；首次运行退出 0，21 个 leaf、32,012 bytes；坏语法 `CONFIG_SYNTAX`、过期证书 `TLS_INVALID`、启动项 `RESTART_REQUIRED` 的旧状态回滚及有效证书替换被观察。重复正文验证不证明 cache hit；该 scope 自身仍列 proxy、WS、SSE、Collector、并发及 drain 待验证 | `A/combination-static/report.json`、`A/012-combination-static.log` |
| expanded all-P1：同一实例启用组合配置，TLS 流、watch、代理和 Collector；执行根目录 A/B、坏配置、真实过期证书与恢复 | 经复审后的授权重跑退出 0，20 个 leaf，报告 pending 空；同 PID 旧 generation 1 的 TLS WS 101 完成，SSE 200 在主动 transport close 后记为 error，不称 completed。主代理 18081 和独立 `example.test`/proxy 18082 故障序列 200/502/200，三组 root/access ID 关联 | `A/combination-expanded/report.json`、`A/012-combination-expanded.log` |
| expanded 并发与停止 | C4 的 100 请求真实 200；两 CLI 并发启动均退出 0、generation 7/8，BUSY 0，不断言 coordinator 执行重叠。16 MiB 大写 L 正文 SHA-256 `289184e1081dba91206603d04683de839d8cceeb3bef6a56badf3dc904bb4043`，Rill 退出 0、socket 消失；完成时间 1793795471533 大于 shutdown 1793795468795，仅说明同 wall clock 日志顺序，不证明内核层全程连续 | 同上 |

static 与 expanded 是不同 scope，不能将其串成跨两次运行的同一 PID。观测和收敛是有限窗口采样；日志和 tracing 为有界 best-effort，不证明零丢失或全时序一致。

## systemd 隔离实测

`A/systemd/results.json`、`A/systemd/manifest.json` 和 `A/012-systemd.log` 记录本次 full 运行退出 0。UID/GID 65534、capability 0，cgroup 512 MiB、CPU 200%、64 tasks；runtime 0700、socket 0600。严格 TLS 代理 Host `example.test`，cache 10→20 到 generation 2，手动 reload 到 generation 3；坏配置、证书错误和启动项分别退出 1，报 `CONFIG_SYNTAX`、`TLS_INVALID`、`RESTART_REQUIRED`；恢复 cache 10 到 generation 8。UID 1 实际尝试控制 socket 退出 3，属于本次证据，不是 008 历史推断。停止时完整排空 16 MiB，SHA-256 同上，Rill 停止，`Result=success`、`ExecMainStatus=0`，unit 清理。

该 fixture 使用 trusted launcher 和隔离覆盖，并非逐字运行生产原 unit；不证明跨发行版、rootless 或 Rust 1.95 MSRV。

## Podman 隔离实测

`A/podman/manifest.json`、`A/podman/results.json` 和 `A/012-podman.log` 记录授权单次重跑退出 0，终态 `complete`，`basic_passed=true`、`full_passed=true`；206 个文件的大小和 SHA-256 已核对，共 426,349 bytes。UID/GID 10001、各 capability 集合为零、只读根、network none；cgroup 为 512 MiB、2 CPU、64 tasks、CPU 6–7。watch sampler 在变更前确认旧 tuple，再观察 cache 10→20、generation 1→2；手动 reload 退出 0、generation 3。负例精确返回 `1 CONFIG_SYNTAX`、`1 TLS_INVALID`、`1 RESTART_REQUIRED`，UID 1 控制访问退出 3；fixture 最终 hash 恢复。HTTPS upstream 的 backend 实际记录 TLS 和 Host `example.test`，metrics 返回 200。

16 MiB 大写 L 下载的 SHA-256 同上，Rill 退出 0，socket 与所记录 cgroup 消失；捕获的 container 和带本次 nonce 的 image 按 ID 清理，删除命令退出 0。不据此声称所有历史 storage 无残留；本次仅验证受控 rootful Podman，不推广为 rootless 或生产部署保证。

## 保留的失败与重试边界

以下为实际保留记录，不据此推断产品 bug，也不抹去失败：

| 记录（均在 A 下） | 原因及后续 |
|---|---|
| `combination-flow-setup-as-error/`、`012-combination-flow-setup-as-error.log` | 外层 AS hard 2 GiB 拒绝 Collector 的 8 GiB 需求；调整外层至 8 GiB，Rill 自身仍 2 GiB |
| `combination-expanded-fault-stop-timeout/`、`012-combination-expanded-fault-stop-timeout.log` | fault-stop 5 秒超时；Python 3.13 `server.wait_closed` 等待 idle，显式授权仅 helper budget 改 8 秒后重跑，结果见 expanded |
| `systemd-initial-pgrp0-error/`、`012-systemd-initial-pgrp0-error.log` | 通用 parser 拒绝内核 PG 0 |
| `systemd-invalid-profile/`、`012-systemd-invalid-profile.log` | profile 中无效 cache-control 改用 snippet |
| `systemd-header-case-error/`、`012-systemd-header-case-error.log` | header 字典大小写问题，后规范化 |
| `podman-build-filelimit-error/`、`012-podman-build-filelimit-error.log` | 1 MiB 文件限制拒绝约 21.8 MiB copy；修复受控 CLI 文件上限为 32 MiB，并保留 stdout 1 MiB 读取拒绝；显式授权单次重跑通过，原失败结果保留 |

Podman 使用已有缓存 base 派生本地无 tag 构建，未安装工具或 pull 新镜像。

## 最终门禁与交接

- systemd 与 Podman 的完整隔离实测、部署报告及独立文档复审已完成，T03/AC03 已验收。
- 正式性能 162 轮在线测量及独立原始核验均已完成：27,936,022 个测量请求、1,548,773 个预热请求、711,813 条流事件，逻辑及预热错误为 0；120 次条件重试、112 轮 client-limit warning 保留。844 个原始文件共 557,046,596 bytes，核验报告 SHA-256 为 `4ded75680521d42d0ee62256fbc6b0adf4811a71a5ec0b04c625543d913f494f`。性能差异分析及验收见 [012-performance.md](012-performance.md)；不得据此宣称容量或零传输错误。
- 最终 `fmt`、`check`、`clippy` 和含 ignored 的完整 176 项测试已通过，见 `data/012/final-quality.log`。首次最终门禁因只读输入副本目录不可遍历而未运行测试，原日志另存 `final-quality-input-permission-error.log`；仅修正专用源码副本权限并明确授权重跑，源码和测试字节未变。
- 最终独立交付复审 `633d7505` 无阻断 H/M；父已同步状态措辞并验收 T04/AC04/G07。阶段独立提交由父创建，用户新增 `AGENTS.md` 与 `docs/gap-analysis-vs-caddy.md` 不纳入本阶段。
