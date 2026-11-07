# 024：组合验收与交付

基线 HEAD `a8168db`；开始时工作区及暂存区干净。`git log --oneline` 核对 001–009 均有独立完成提交，依次为 `1bc02e9`、`7bd1c13`、`316f7b7`、`7fe2ab8`、`6770615`、`18ebbb9`、`81da048`、`7ba9898`、`a8168db`。010 已完成独立复审及最终验收，完成提交以包含本记录的 Git 提交为准。

## 本轮质量门禁

工具链：`rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`；未验证 1.95 MSRV。

- `cargo fmt --all --check`：通过。
- `cargo check --locked --offline -q`：通过。
- `cargo clippy --locked --offline --all-targets -q -- -D warnings`：通过。
- `cargo build --release --locked --offline -q`：通过。
- 复制完整仓库（排除 `.git`、`.pi`、`target`）至 `/var/tmp/rill010-suite-D5Tn8y`，使用 `CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target cargo test --locked --offline --no-run --message-format=json` 离线构建，按 Cargo JSON 的 test 工件逐一执行 `runuser -u nobody -- <工件> --test-threads=2`：10 个工件，342 passed、4 ignored，0 failed。完整工件清单、构建输出及原始测试输出分别为该目录的 `artifacts.txt`、`build.json`、`build.log`、`gate-*.log`。忽略项依赖专用环境，并未在此批量启用。未修改仓库 `target` 链接。

上述门禁只覆盖本轮已有源码，并不代替新增组合回归、隔离场景、资源测量或最终复审；后续代码更改后须重跑相应门禁。

## 历史缺口与最终收口

以下保留各轮增量、失败和修复；最终独立复审及父代理门禁见末节。历史记录 016–023 只用于追溯，不作为本轮验收。010 各项验收和 G06 已按本轮实际证据确认；未进行生产切换。

## 本轮组合增量

- `cargo test --locked --offline -q --test caddy_parity -- --test-threads=2`：root 环境 1 passed。真实服务同一快照验证无凭据 401、认证 DAV PUT、响应头与正文替换、缺失凭据文件的失败候选保留旧服务、成功 reload 切换头策略而保留文件数据、文件日志 site/route 身份且不包含凭据。同一工件在 0755 隔离副本中经 `runuser -u nobody -- <工件> --test-threads=2` 重跑 1 passed；原始输出 `/var/tmp/rill010-suite-D5Tn8y/caddy-test.log`。仍需执行最终完整 Cargo 门禁。
- 将 `examples/caddy-parity` 复制至 `/var/tmp/rill010-example-v8dDuG`，创建服务运行目录、私有日志/DAV 目录及临时随机令牌；`target/debug/rill check --config .../config.kdl` 通过。真实 `run` 的静态和 SPA GET 返回 200、OPTIONS `/api/x` 返回 204 及配置的 CORS 头、未认证 DAV GET 返回 401；`rill reload --socket .../runtime/control.sock` 返回 generation 2，服务正常 SIGTERM。静态及 CORS 头、静态和 SPA 正文、服务日志保存在该隔离目录；目录含本次随机测试令牌，不应复制到仓库。API、gRPC、动态代理的实际后端请求、条件 304 和 DAV 写入不由该次示例试运行证明。
- 010 逐项状态见 [差距文档当前状态](../gap-analysis-vs-caddy.md#当前状态010-已完成)；016–023 保持历史追溯，不冒称本轮专项已通过。

- 更新隔离副本后执行 `CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="runuser -u nobody --" cargo test --locked --offline --no-fail-fast -q -- --test-threads=2`：Cargo 驱动测试工件以 nobody 运行，主结果 343 passed、4 ignored；doc tests 由 root 启动的 Cargo/rustdoc 执行，0 项。原始完整输出 `/var/tmp/rill010-suite-D5Tn8y/cargo-final.log`。这不是组合专项或资源门禁完成证据。
- `cargo fmt --all --check`、`cargo clippy --locked --offline --test caddy_parity -q -- -D warnings`、`git diff --check`：通过。

## 本轮补充隔离场景与资源实测

- `python3 tests/dynamic_proxy/netns.py`：脚本验证 net/mount namespace 与父进程不同、仅 lo 且无路由后，在私有 namespace 中设置临时地址；真实服务通过 15 项断言，包括认证动态目标 HTML 替换、WS/SSE 跨 reload、128 活动请求及额外请求容量拒绝。`/var/tmp/rill010-evidence/dynamic-transform.log` 有 PASS 行及后端关闭连接的 BrokenPipe 堆栈，进程退出 0；首次重跑未包含新增替换场景，保留为 `dynamic.log`，不计该场景证据。没有外网访问。
- `python3 tests/feasibility/grpc_container/run.py --rill`：使用本地已有镜像且 `--pull=never`，真实 h2c/TLS h2 通用 gRPC 四类调用、背压、失败及成功 reload 后旧双向流继续、新请求切换后端通过；脚本报告容器和隔离网络均清理。原始输出 `/var/tmp/rill010-evidence/grpc.log`。不是 OTLP 验收。
- 临时复制示例并仅调整本次两个空闲 loopback 端口，真实 Python HTTP 后端返回 404 且代理响应保留 CORS 头，静态 ETag 的 If-None-Match 返回 304；证据 `/var/tmp/rill010-evidence/api.log`。随后在另一临时副本使用实际 loopback HTTP 后端请求 `/api/item`，验证状态 200、正文 `api-ok` 和 CORS 头；原始输出 `/var/tmp/rill010-evidence/api-success.log`。两次后端、服务和临时凭据目录均已清理。
- `cargo test --locked --offline -q --test reload failed_try_files_root_reload_preserves_old_snapshot`、`... file_log_failed_reload_keeps_old_generation_and_sink`、`... reload::streams -- --test-threads=2`、`cargo test --locked --offline -q --test webdav webdav_registry_survives_removal_failed_activation_and_reload_during_put_and_move`：分别 1、1、6、1 项通过；覆盖坏 root/日志候选、WS/SSE 和文件日志长流快照及 DAV 锁/写入跨 reload。原始输出 `/var/tmp/rill010-evidence/reload.log`；这组定向测试本次以 root 运行，不替代非 root 最终门禁。

### 本机负载采样（非容量承诺）

使用现有 release 二进制与临时 Python 标准库客户端，对 loopback 单站点固定响应发送等量请求，服务及客户端在同一宿主。关闭组为无过滤及无文件日志；开启组增加 text/plain 字面量替换和站点文件日志。两组各 8 个客户端、每客户端 100 个 keepalive GET、每响应 4096 字节，实际各 800 个响应、3,276,800 正文字节、零错误；不包含 API、DAV、gRPC 或动态代理工作量。压力组开启同样增强功能，128 客户端各 20 请求，共 2560 响应、10,485,760 字节、零错误；客户端并发数不是服务器同时活动请求的证明。

每约 10 ms 从被测进程 `/proc/<pid>/stat` 采样 user+system CPU tick，从 `status` 采样 VmRSS、从 `fd` 计数；CPU 差包含负载前后各约 50/300 ms 采样窗口，墙钟只计客户端负载，短负载和采样间隔可能错过瞬时峰值。CPU 秒可因多线程大于墙钟，RSS 是采样进程驻留而非配额，FD 是采样峰值。延迟是客户端从发送请求到读完响应的毫秒数，p99 为排序后第 99 百分位样本；磁盘为采样后日志文件实际大小，不含配置及文件系统元数据。

| 组 | 墙钟 s | CPU s | RSS 峰值 KiB | FD 峰值 | 日志文件/字节/行 | p50/p99/max ms | 错误/日志丢弃诊断 |
| --- | ---: | ---: | ---: | ---: | --- | --- | --- |
| 关闭 | 0.1358 | 0.07 | 18664 | 20 | 0 / 0 / 0 | 1.045 / 3.754 / 5.366 | 0 / 0 |
| 开启 | 0.1530 | 0.55 | 19744 | 22 | 1 / 96800 / 800 | 1.361 / 3.099 / 4.399 | 0 / 0 |
| 压力 | 0.4737 | 1.75 | 21976 | 71 | 1 / 309760 / 2560 | 6.813 / 34.152 / 69.934 | 0 / 0 |

原始汇总 `/var/tmp/rill010-evidence/measurement.jsonl`、`pressure.jsonl`，逐次进程采样分别为 `off/samples.json`、`on/samples.json`、`pressure/samples.json`；采样命令为同目录一次性 `measure.py`、`pressure.py`。三次服务结束时均 SIGTERM 并等待退出。压力组观察到延迟上升和更高 CPU/FD，无法据此区分客户端调度、替换、日志及服务端排队的占比；该负载没有观察到日志丢弃，不声称饱和或推导 RPS 容量。

队列另以 `cargo test --locked --offline -q --lib saturated_queue_drops_without_waiting_for_real_file_worker -- --nocapture` 实测受控真实文件 worker：持锁阻塞写线程，1024 次 submit 成功、下一次返回丢弃且计数为 1，输出 `LOG_DROPPED`；见 `/var/tmp/rill010-evidence/queue.log`。这是库级受控锁竞争，不是外部指标、慢盘或以上真实服务负载的队列深度测量。动态 128 活动请求拒绝另有下述独立采样；不能推断其他限额压力条件的资源峰值。007 共享内存信用不含 codec/既有 IO 等，不能解释为 RSS 硬限；009 磁盘信用只覆盖进程寿命，用户态关闭等待与 D 态回收边界不变。

- 资源上限补充采样：在现有 namespace 场景的一次性副本 `netns-sample.py` 中，以约 10 ms 间隔采样真实 Rill，先保留 SSE 和 WebSocket，再依次保持 126 个动态目标请求，额外第 129 请求实际返回 503；126 个最终响应均为 302，共 24,868 字节（含 HTTP 响应头），拒绝请求耗时 0.634 ms。采样 79 次，窗口墙钟 0.8464 s、CPU tick 差 0.22 s、VmRSS 采样峰值 42,468 KiB、FD 采样峰值 295。该配置不启用文件日志，磁盘日志文件/字节为 0；队列深度及 SSE/WS 各帧延迟未采样。原始数据及逐点样本在 `/var/tmp/rill010-evidence/dynamic-resource.log` 的 `PRESSURE` 行；场景包含其他预备请求，数值只描述该采样窗口，不是独立请求吞吐或全功能资源上限。脚本仅用私有 net/mount namespace，不对外联网。
- 增量后重新执行 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo build --release --locked --offline -q` 与 `git diff --check`，均通过。完整副本更新后再次用 `CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="runuser -u nobody --" cargo test --locked --offline --no-fail-fast -q -- --test-threads=2` 通过：主测试 343 passed、4 ignored；Cargo/rustdoc root 执行 0 个 doc test；原始输出 `/var/tmp/rill010-suite-D5Tn8y/cargo-final-2.log`。最终复审和父代理重验仍待执行。

## 复审阻断后的本轮补充

### 四项专用 ignored 测试

- 隔离 DNS：`unshare --net --mount --fork`，仅在新 namespace 启用 lo、确认无默认路由，临时私有 bind 的 resolv.conf 为 `nameserver 127.0.0.1`、`options timeout:30 attempts:5`；`cargo test --locked --offline -q --test reload timed_out_prepare_keeps_admission_and_never_publishes_late -- --ignored --nocapture --test-threads=1`：1 passed，32.21 s，输出 `DNS_NAMESPACE_CLEANUP_OK`。原始 `closure/dns-long.log`。此前 `timeout:1 attempts:1` 在 1.05 s 返回而断言失败，`timeout:5 attempts:2` 在 10.06 s 返回非 PREPARE_TIMEOUT 而失败；分别保留 `dns.log`、`dns-retry.log`，不计通过。
- DAV 满盘：`cargo test --locked --offline -q --test webdav isolated_tmpfs_enospc -- --ignored --nocapture --test-threads=1`：1 passed；测试自行核验私有 mount namespace，1 MiB tmpfs ENOSPC 返回 507、旧目标不变、临时文件清理，bind 别名 GET/PUT 均 409，退出前卸载。`closure/dav-enospc.log`。
- 日志满盘：在 `unshare --mount --fork` 私有传播 namespace 中新建并仅挂载本次 1 MiB tmpfs，设置 `RILL_LOG_ENOSPC_DIR` 后单独运行 `cargo test --locked --offline -q --lib enospc_partial_record_is_rolled_back_before_next_record -- --ignored --nocapture --test-threads=1`：1 passed；输出 `LOG_TMPFS_CLEANUP_OK`，记录 `closure/log-enospc.log`。
- cadaver/rclone：获用户本次专门授权后，从官方 Debian trixie 源仅下载 `cadaver=0.26+dfsg-2`、`libneon27t64-gnutls=0.34.2-1` 两个包到本次临时目录，SHA-256 与 Debian 包索引及 022 历史记录逐文件核对通过，以 `dpkg-deb -x` 解包，不安装或升级。`LD_LIBRARY_PATH` 仅作用于测试客户端；以 Cargo runner 的 nobody 运行 `cargo test --locked --offline -q --test webdav real_cadaver_and_rclone -- --ignored --nocapture --test-threads=1`：1 passed，cadaver 0.26 与 rclone v1.60.1-DEV 的 loopback 方法、属性、锁、读写字节比对通过。下载、校验、ldd、测试及清理输出分别在 `closure/cadaver-download.log`、`cadaver-checksums.txt`、`cadaver-ldd.log`、`dav-clients.log`、`cadaver-cleanup.log`；临时下载/解包目录已删除。仅下载这两个授权工件，未把包、凭据或校验值提交仓库。

上述 `closure/` 均为 `/var/tmp/rill010-evidence/closure/`。四项分别执行，不批量启用 ignored；DNS 和两个满盘测试因 namespace/mount 所需权限以 root 运行，客户端以 nobody 运行。

### 本轮逐项证据索引

表中 `named-*.log` 是本轮以 Cargo runner `runuser -u nobody --` 执行相应完整 test 工件且逐项显示测试名的原始输出，位于 `/var/tmp/rill010-evidence/closure/`。请求与预期是对应源码断言；输出中的 `ok` 是本轮实际通过，不以 016–023 替代。具体测试名保持源码拼写，以便核对。

| 条目及批准子行为 | 本轮请求、预期与结果 | 测试与原始输出 |
| --- | --- | --- |
| 2.1 头 set/add/delete/replace、Vary、Host、真实客户 IP、MIME/状态条件 | 代理、静态和本地响应分别核对安全/CORS 头、Location、Vary；上游核对 Host 和可信/不可信客户 IP；禁止头及尚无上游返回 503；全部断言通过 | `chain_header_policy_across_terminal_responses`、`chain_headers_and_trusted_client_address`、`forwarded_and_hop_headers_are_rebuilt_without_changing_the_raw_target`，`named-http_proxy.log`；示例 204、API 200/404 CORS `api-success.log`、`api.log` |
| 2.2 respond/预检/状态 | `/empty` 204、`/cached` 304、HEAD 保留长度但无正文、未匹配 404，固定文本 200；通过 | `chain_rules_responses_and_existing_actions`，`named-http_proxy.log`；实际预检 `static.headers` 同级的 `cors.headers` |
| 2.3 路径剥离、rewrite、try-files、错误链、顺序/互斥 | `/assets/file.txt` 返回文件，`/change/value?x=%2f&x=2` 到上游保留查询，缺失 404，目录/符号链接/FIFO 403、编码越界 400；404/405 错误链保留状态并重写页面；均通过 | `chain_paths_rewrite_strip_and_constrained_candidates`、`chain_error_handlers_only_catch_local_failures`、`chain_branch_captures_inherit_and_override`，`named-http_proxy.log` |
| 2.4 all/any/not、方法、头等值/前缀、glob/正则、CIDR、错误状态 | 正则/路径、可信 CIDR 和重复头等值选路；本轮新增真实请求：gRPC MIME 前缀 POST 与 X-Choice GET 返回 201，JSON POST 与 DELETE 返回 404；404/405 错误条件按状态选页；通过 | `chain_rules_responses_and_existing_actions`、`chain_headers_and_trusted_client_address`、`chain_error_handlers_only_catch_local_failures`，`named-http_proxy.log`；`header_prefix_and_boolean_matchers_route_real_requests`，`closure/matchers.log`（root 定向，最终非 root 完整测试另记） |
| 2.5 固定上游选择、Host、h2c/TLS h2、gRPC | 按匹配器转发并核对 Host；真实 h2c/TLS h2 一元、客户端流、服务端流、双向流、trailers、取消/背压及旧流跨 reload 均通过 | `chain_header_policy_across_terminal_responses`、`h2c_chain_proxy_uses_real_h2_upstream`，`named-http_proxy.log`；容器通用 gRPC `grpc.log` |
| 2.6 bcrypt/Argon2、令牌及 DAV 安全/状态 | 无认证 401、正确凭据与错误凭据分流；真实 PUT、属性/锁、COPY/MOVE，越权目的地及链接拒绝、深度与资源限制、跨 reload 保锁和写状态；真实 ENOSPC 507 与客户端互通通过 | `authentication_basic_and_token_enforce_and_clear_credentials`、`authentication_argon2_and_unknown_user`，`named-http_proxy.log`；`webdav_real_methods_atomic_properties_exclusive_lock_and_head`、`webdav_denies_destination_auth_bypass_symlinks_hardlinks_and_private_static_aliases`、`webdav_propfind_enforces_resource_and_depth_boundaries_without_exposing_internal_names`、`webdav_registry_survives_removal_failed_activation_and_reload_during_put_and_move`，`named-webdav.log`；`dav-enospc.log`、`dav-clients.log` |
| 2.7 动态代理与跨块替换 | 私有 namespace 认证目标 HTML 的 old→new 替换、禁止内网和身份头泄漏、跨 reload/容量边界通过；identity/gzip/zstd 源分块与 literal/regex capture 的 3×3 组合输出相同，截断/尾部垃圾失败 | `dynamic-transform.log`；`pipeline_decodes_validates_and_encodes_with_shared_rules_and_chunk_independence`，`named-body_transform.log`（后者是生产 pipeline 测试，不冒称端到端网络） |
| 2.8 gzip/zstd、旁路与 sidecar | 真实服务的静态/响应变换协商 identity/gzip/zstd，Range/HEAD 和错误链、sidecar 与条件元数据断言通过；源 gzip/zstd 解码后再替换编码的分块矩阵通过 | `respond_static_sidecar_range_head_and_error_chain_apply_actual_declarations`、`encode_only_preserves_sidecar_validators_while_online_and_listing_use_get_metadata`、`proxy_bypass_uses_original_metadata_and_preserves_range_business_conditions`、`pipeline_decodes_validates_and_encodes_with_shared_rules_and_chunk_independence`，`named-body_transform.log` |
| 2.9 按站点文件、大小/数量/时间轮转、字段白名单、stderr、候选/队列 | 文件日志字段、路径去查询、坏日志候选保持旧 sink；真实满盘记录恢复、队列容量/丢弃、进程磁盘信用回归通过；轮转时间/数量测试见本轮 lib 命名输出 | `site_file_log_check_is_read_only_and_logs_static_rule_identity`、`explicit_fields_use_path_without_query_and_validated_client_ip`，`named-http_proxy.log`；`reload.log`、`log-enospc.log`、`queue-resources.log`、`named-observability.log` |
| 2.10 仅日志配置身份 | 站点标签 `integration` 与规则 `dav` 记入文件，不包含令牌；字段选择按配置身份而非原始 Host/URI | `authenticated_dav_transform_and_log_survive_failed_candidate_and_reload`，`named-caddy_parity.log`；`site_file_log_check_is_read_only_and_logs_static_rule_identity`，`named-http_proxy.log` |

### 资源采样复审修正

`/var/tmp/rill010-evidence/measure.py` 与 `pressure.py` 已修正为每轮 `mkdtemp` 全新目录，不复用/清理其他轮目录；分别以 `python3 /var/tmp/rill010-evidence/measure.py` 和 `python3 /var/tmp/rill010-evidence/pressure.py` 重跑。原始输出 `closure/measure-new.log`、`closure/pressure-new.log`，含实际 RUN_DIR 和逐点 samples.json。新结果：关闭/开启各 800 请求、8 客户端、3276800 正文字节、0 错误，墙钟 0.1324/0.1765 s、采样 CPU 0.08/0.52 s、VmRSS 峰值 18644/20352 KiB、FD 峰值 20/23、日志 0/96800 字节、p50 1.090/1.552 ms、p99 3.413/3.972 ms、最大 6.830/5.209 ms。128 客户端压力组 2560 请求、10485760 正文字节、0 错误，墙钟 0.7205 s、CPU 1.64 s、RSS 23928 KiB、FD 82、日志 309760 字节、p50/p99/max 10.979/49.262/82.314 ms；这仍不证明同时活动请求数。

同一队列饱和场景补充资源口径：`closure/queue-sample.py` 以约 1 ms 采样实际测试进程 `/proc`，现有测试只增加测试内观测输出；持锁阻塞文件 worker 时提交 1024 条日志记录均接受、第 1025 条被拒绝，实际 dropped 计数 1，提交总耗时 10338 µs、拒绝耗时 1 µs；阻塞时实际 1 个文件 0 字节、关闭后 1 个文件 4600 字节。总墙钟 0.0309 s、采样 CPU 0.02 s、RSS 峰值 20208 KiB、FD 峰值 6、25 个样本；`closure/queue-resources.log` 和 `queue-samples.json`。这是日志记录提交而非 HTTP 请求，不声称 1024 条最终全部落盘，也不把这些微秒延迟称为 HTTP 延迟。动态限额重跑的完整采样包装脚本现保存为 `closure/netns-sample.py`，从仓库根执行 `python3 /var/tmp/rill010-evidence/closure/netns-sample.py`；新输出 `closure/dynamic-resource.log`。两项分别是日志队列容量和动态请求容量，不混为同一 HTTP 场景，均不代表生产容量或 RSS 硬限。

队列采样重放前，从仓库根重新构建测试工件并更新索引，不能沿用缓存中的哈希路径：

```sh
cargo test --locked --offline --lib --no-run --message-format=json > /var/tmp/rill010-evidence/closure/lib-build.json
python3 - <<'PY'
import json
from pathlib import Path
root = Path('/var/tmp/rill010-evidence/closure')
artifacts = [json.loads(line) for line in (root / 'lib-build.json').read_text().splitlines()]
executables = [item['executable'] for item in artifacts
               if item.get('reason') == 'compiler-artifact'
               and item.get('profile', {}).get('test')
               and 'lib' in item.get('target', {}).get('kind', [])
               and item.get('executable')]
assert len(executables) == 1
(root / 'lib-exe').write_text(executables[0])
PY
python3 /var/tmp/rill010-evidence/closure/queue-sample.py
```

该命令只更新本次证据目录的工件索引；原始采样结果在重放前应另存，重放输出不当作之前的同一轮测量。执行测试所用身份应如实记录。

- 最后代码改动后重新通过 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo build --release --locked --offline -q`、`git diff --check`。隔离副本以 `CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="runuser -u nobody --" cargo test --locked --offline --no-fail-fast -q -- --test-threads=2` 实际执行：344 passed、4 ignored；root 启动 Cargo/rustdoc 的 doc tests 为 0 项。四个 ignored 已按上文逐项单独通过；完整输出 `closure/cargo-final-3.log`。非 root `cargo test --locked --offline --lib observability:: -- --test-threads=2` 命名结果 23 passed、1 ignored（日志 tmpfs 已独立执行），输出 `closure/named-observability.log`。该轮之后父代理完成最终复核，结果见下节。

## 主会话最终复核与交接

主会话实际核对 Git：001–009 独立完成提交与本记录九个哈希一致。最终复审指出在线 gzip 测试曾允许 identity 回退，现对真实服务 `/local` 的 `Accept-Encoding: gzip` 请求明确断言 `Content-Encoding: gzip`，再用 GzDecoder 解码核对正文；定向测试 1 passed，不以 sidecar 或内存 pipeline 替代在线 gzip。

修改后主会话重新执行 fmt、离线 check、全目标 clippy `-D warnings`、release 构建和 `git diff --check`，全部通过。将完整仓库排除 `.git`、`.pi`、`target` 复制至 0755 `/var/tmp/rill010-acceptedgate-8lvjtq0t`，使用既有外部 target 和 `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="runuser -u nobody --"` 执行 `cargo test --locked --offline --no-fail-fast -q -- --test-threads=2`：344 passed、4 ignored、0 failed；root 驱动 Cargo/rustdoc 的文档测试 0 项。完整输出位于该目录 `cargo-final.log`。四项 ignored 的本轮专项通过证据见上文，不称默认套件自动通过。首次 gzip 断言未符合 rustfmt，按格式修正后重跑全部门禁；失败命令不算成功证据。

独立 reviewer 最终只读复核为 `No issues found`、`Merge verdict: OK`，解除 gzip 验收及工件路径复现阻断，并核对最终测试输出；未冒称 reviewer 运行全部测试。任务、验收及 G06 据此同步完成。交付仅覆盖冻结契约：不新增 OTLP/metrics，不承诺完整 Caddy 语法兼容或容量，不切换现网；生产部署仍须单独授权。用户已接受的共享内存计量、DAV 提交结果未知及日志 D 态回收边界保持不变。
