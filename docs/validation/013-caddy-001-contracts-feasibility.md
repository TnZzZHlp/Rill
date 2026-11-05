# 001 契约冻结与技术可行性验证记录

本记录对应子计划 001（`docs/plans/caddy-feature-parity/subplans/001-contracts-feasibility.md`）。它记录 001-T01 基线核对、001-T02 冻结契约、001-T03 隔离可行性验证的实际命令与原始结果。001-T04 用户验收尚未取得：本文件不构成验收通过，也不解除 001 关卡。

## 1. 基线与环境

| 项 | 实测值 |
| --- | --- |
| HEAD | `450722f36a7297c826e7ac68a681f4e7496cf7d1` `refactor(observability): remove OTLP tracing and metrics` |
| 原计划 012 完成提交 | `c5e6fa7` `feat(rill-web-server): complete subplan 012 - release-validation` |
| 工具链 | `rustc 1.99.1 (58cf8f9c3 2026-10-13)`、`cargo 1.99.1 (5f94df478 2026-08-27)`；未验证 1.95 |
| 构建缓存 | `target` 解析为 `/root/.cache/mbx/targets/v1/a957...` 外部共享缓存，未删除或替换 |
| 工作区 | 跟踪树干净；未跟踪 `AGENTS.md`、`docs/gap-analysis-vs-caddy.md`、`docs/plans/caddy-feature-parity/` 及本次新增验证文件 |
| 网络 | 全部命令使用 `--offline`，依赖取自本地 cargo 缓存；未访问任何线上服务，未操作生产 Caddy |
| 特权和隔离 | 以 UID 0 运行；`/root` 为 0700，权限拒绝类用例需要非特权身份，见第 4 节 |

观测功能移除后的实际观测面（读源码核对，不引用历史文档）：

- `src/observability/` 仅有 `access.rs`、`identity.rs`、`logging.rs`、`mod.rs`、`owner.rs`、`request.rs`，没有 metrics、OTLP 或 tracing 代码。
- `docs/configuration.md` 第 195 行说明 `metrics`、`tracing`、`trace-sample-per-mille`、`otlp-endpoint` 均为未知字段并被拒绝，服务不创建指标监听或 exporter。
- 历史 RouteId 身份是 SHA-256 前 16 字节、渲染为 32 个十六进制字符（`src/observability/identity.rs`），不是差距文档所述的 16 位；且指标端点已不存在，该描述整体失效。

差距文档中不得作为交付证据的估计：25 个域名来自磁盘配置调查而非在线验收；“约 12 个可迁移”没有逐站验收；606 MiB 是目录大小而不是响应正文大小；未扫描所有静态 root 的侧车文件。

## 2. 差距追溯矩阵

| 差距 | 批准范围 | 责任阶段 | 契约章节 |
| --- | --- | --- | --- |
| 2.1 | 请求/响应头设置、追加、删除、受控替换、Vary 合并、Location、真实客户 IP、上游 Host、响应 MIME/状态条件 | 003 | CONTRACT.md 3.3、4 |
| 2.2 | respond 状态/文本、204 预检、401/404、校验端点 | 002 | 3.4、4 |
| 2.3 | 显式前缀剥离、rewrite、try-files、错误链、serve-files、互斥与顺序 | 002、003 | 3.5、4、7 |
| 2.4 | all/any/not、方法、头等值/前缀、路径 glob/正则、客户端 CIDR、错误状态 | 002、003 | 3.2、4 |
| 2.5 | 固定上游选择、上游 Host 改写、h2c/TLS h2 与完整 gRPC | 002、003、005 | 3.6、6 |
| 2.6 | bcrypt/Argon2 Basic Auth、静态令牌、内置 WebDAV | 004、008 | 3.7、3.8、6 |
| 2.7 | 动态目标代理、顺序字面量/正则正文替换 | 006、007 | 3.9、3.10、5 |
| 2.8 | gzip/zstd 在线编码、既有 sidecar 协调 | 007 | 3.10、4 |
| 2.9 | 按站点文件日志、轮转、字段白名单、stderr | 009 | 3.11、5、6 |
| 2.10 | 仅日志中的配置来源可读身份 | 009 | 3.11、6 |
| 2.11 | 全部排除 | 无 | 1 |

排除项：OTLP、tracing、所有新增 metrics 能力与指标身份映射；2.11 全部内容；生产切换。通用 gRPC 代理在范围内。

## 3. 冻结契约

契约正文位于 [`docs/plans/caddy-feature-parity/CONTRACT.md`](../plans/caddy-feature-parity/CONTRACT.md)。本节只记录被测事实：配置示例的 KDL v2 可解析性由 `tests/feasibility/contract.rs` 直接读取该文档并逐块解析；契约中的每一个新限额、错误码和接口草图均在该文件，未在其他文档重复定义。

## 4. 隔离可行性验证

运行命令（在仓库根目录）：

```sh
cargo test --locked --offline --test feasibility -- --test-threads=1
```

修复前原始结果（25 项，含 2 项显式忽略；当前结果见 4.4 和第 5 节）：

```text
test feasibility::configuration::configuration_contract_examples_parse_as_kdl_v2 ... ok
test feasibility::configuration::v2_rejects_legacy_bare_booleans ... ok
test feasibility::contract::frozen_contract_examples_parse_as_kdl_v2 ... ok
test feasibility::logging::a_committed_worker_keeps_writing_until_its_owner_releases_it ... ok
test feasibility::logging::a_rejected_candidate_never_replaces_the_previous_active_file ... ok
test feasibility::logging::candidate_preparation_creates_no_file ... ok
test feasibility::logging::mid_activation_failure_rolls_back_every_candidate_and_keeps_the_previous_file ... ok
test feasibility::streaming::h2c_cancel_of_one_stream_keeps_the_connection_usable ... ok
test feasibility::streaming::h2c_request_streaming_preserves_chunk_order_and_trailers ... ok
test feasibility::streaming::h2c_unary_call_carries_grpc_trailers ... ok
test feasibility::streaming::literal_match_holds_the_bounded_prefix_until_end_of_stream ... ok
test feasibility::streaming::literal_replacement_output_is_independent_of_input_chunking ... ok
test feasibility::tls::handshake_crossing_publication_keeps_its_original_selection ... ok
test feasibility::tls::new_handshakes_use_new_certificates_without_closing_existing_streams ... ok
test feasibility::tls::pingora_listener_exposes_the_same_callbacks_and_http2_settings ... ok
test feasibility::tls::unknown_sni_cannot_receive_another_sites_certificate ... ok
test feasibility::transport::pingora_streams_before_upstream_completion_and_drains_during_shutdown ... ok
test feasibility::upstream_h2c::pingora_h2c_connector_completion_rate_is_recorded ... ignored, Pingora 0.9.0 h2 connector is intermittently unable to complete a call
test feasibility::upstream_h2c::raw_h2_client_completion_rate_is_recorded ... ignored, comparison control for the connector completion-rate probe
test feasibility::write::constrained_write_commits_atomically_and_leaves_no_temporary_file ... ok
test feasibility::write::constrained_write_refuses_to_escape_the_root_through_a_symlink ... ok
test feasibility::write::exclusive_locks_survive_a_reload_and_die_with_the_process_registry ... ok
test feasibility::write::lock_ttl_expiry_frees_the_slot_without_an_unlock ... ok
test feasibility::write::o_path_without_follow_yields_the_symlink_itself_so_writers_must_check_the_type ... ok
test feasibility::write::storage_identity_is_the_device_and_inode_of_the_prepared_root ... ok
test result: ok. 23 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 1.69s
```

上列为历史结果；4.4 的修复已移除两个探针的 ignore，当前默认运行全部 25 项。

### 4.1 分块无关性（001-T03 字面量；正则未验证）

`tests/feasibility/streaming.rs` 用一个 42 字节输入，按全部 43 个二分点和 10 个固定步长（1、2、3、5、7、11、13、17、19、23 字节）重新分块，断言输出逐字节相同；并单独验证跨块匹配、待决前缀延迟到 EOF 才输出、不完整尾部按原样输出。证据：`literal_replacement_output_is_independent_of_input_chunking` 与 `literal_match_holds_the_bounded_prefix_until_end_of_stream` 通过。

实测范围仅限字面量：被测算法持有 `模式长度 - 1` 个字节的待决前缀并只在 EOF 冲刷，因此对字面量可以证明输出与分块无关。**正则捕获替换未在本阶段实测**：`regex 1.13.1` 只是传递依赖，未提升为直接依赖；其增量扫描、左最先匹配与 `$1` 替换是否满足同一分块无关性质，仍需在 007 前的独立验证中给出同等证据。CONTRACT.md 3.10 的规则因此仍是待验证契约，不是已证结论。

### 4.2 HTTP/2 与 gRPC 协议面

`h2c_unary_call_carries_grpc_trailers`、`h2c_request_streaming_preserves_chunk_order_and_trailers`、`h2c_cancel_of_one_stream_keeps_the_connection_usable` 通过真实 loopback `h2` 连接（`h2 0.4.20` 客户端与服务端握手）验证：

- 请求与响应都携带 `content-type: application/grpc`，响应以 DATA + trailers 结束，`grpc-status: 0` 与 `grpc-message: ok` 逐项断言。
- 请求侧三块增量写入被后端按三个独立的 DATA 帧接收（响应头 `x-request-frames: 3` 断言），并按序拼接为 `stream:onetwothree`。这证明客户端侧不聚合分块、帧边界保持；它**不**证明 Rill 未来的转发路径不聚合。
- 取消一个流后，同一连接上的兄弟流仍被正常服务（`h2c_cancel_of_one_stream_keeps_the_connection_usable` 断言后续调用返回 200 与 `unary:payload`），说明流级取消不污染连接。

源码侧确认（不等于端到端证明）：`pingora-core-0.9.0/src/upstreams/peer.rs` 有 `PeerOptions::set_http_version(max, min)`；`protocols/http/v2/client.rs` 暴露 `write_request_header`、`write_request_body`、`read_response_header`、`read_response_body`、`read_trailers`；`pingora-proxy-0.9.0/src/proxy_h2.rs` 处理上游 trailers（约 993–1021、1238–1264 行），而 `proxy_h1.rs` 标注 `TODO: support trailers for h1`。

### 4.3 WebDAV 受约束写入

`tests/feasibility/write.rs` 使用 `openat2` + `RESOLVE_BENEATH|RESOLVE_NO_MAGICLINKS` 完成真实文件写入：

- 同目录临时文件 + `renameat` 原子提交后，目录里只剩目标文件，无残留临时文件。
- 目录被替换成指向外部目录的符号链接时写入以 `EXDEV` 被拒绝，外部目录保持空。
- `O_PATH|O_NOFOLLOW` 打开符号链接条目得到的是链接本身（`file_type().is_symlink()`），必须再用 `O_NOFOLLOW` 数据打开才会得到 `ELOOP`；这是 008 必须检查类型的实测依据。
- 独占锁模型在同一 registry 内拒绝第二个持有者（`DAV_LOCKED`）、达到上限时返回 `DAV_LIMIT`、TTL 到期释放槽位；新进程 registry 中同一 storage-id 没有旧锁，符合“跨 reload 保锁、重启失锁”。
- 同一路径在同一文件系统重新打开得到相同 `(st_dev, st_ino)`，替换目录后不同，可作为稳定存储身份。

### 4.4 已修复：测试后端未持续驱动 h2 连接

原报告把探针超时记为 Pingora h2 连接器阻断，该结论撤回。修复前再次复现：同一十次探针 `ok=4 fail=6`，且测试仍返回成功，因为断言只检查完成数与失败数之和为 10。

根因在测试后端：`connection.accept().await` 返回请求后，代码在同一个循环中等待 `RecvStream::data()`。h2 服务端 Connection 必须继续被轮询才能接收 DATA、处理流控与发送响应；后端停止轮询连接时，是否恰好已读到完整请求体取决于报文到达时序。直接 h2 客户端在先前探针上恰好通过，不能排除测试夹具错误。

修复 `upstream_h2c.rs` 和 `streaming.rs`：每个请求的读体及响应在 JoinSet 子任务中执行，接受循环持续轮询 Connection，连接结束后收集子任务结果。两个连接器/对照探针移除 ignore；Pingora 探针改为要求十次全部成功，而非只要求尝试总数为十。没有修改 Pingora、Tokio、生产代码或依赖。

修复后复现命令：

```sh
cargo test --locked --offline --test feasibility upstream_h2c -- --nocapture --test-threads=2
for i in $(seq 1 10); do
    cargo test --locked --offline --test feasibility upstream_h2c -q -- --test-threads=2 || exit 1
done
```

实测输出摘要：

```text
pingora connector: ok=10 fail=0
raw h2 client: ok=10 fail=0
round 1..10: 每轮 2 passed; 0 failed; 0 ignored
```

重复十轮共 100 次 Pingora 调用、100 次直接 h2 调用，全部成功；默认 feasibility 套件 25 passed、0 ignored。证明范围仅为此 h2c 一元连接器探针，不能替代 TLS h2、完整 gRPC 四类调用、取消传播或连接池信任隔离验证。

以下保留修复前的观察作为历史记录，不再作为协议阻断依据：

`tests/feasibility/upstream_h2c.rs` 包含两个成对的 `#[ignore]` 探针，拓扑、负载和后端实现完全相同，唯一差别是客户端：

- `pingora_h2c_connector_completion_rate_is_recorded`：用 Pingora `Connector::get_http_session`（`PeerOptions::set_http_version(2, 2)`）打开 h2c 会话。
- `raw_h2_client_completion_rate_is_recorded`：直接用 `h2::client::handshake`。

复现命令：

```sh
cargo test --locked --offline --test feasibility -- --test-threads=1 --ignored --nocapture
```

原始输出（本次运行）：

```text
attempt 1: failed deadline has elapsed
attempt 2: failed deadline has elapsed
attempt 3: failed deadline has elapsed
attempt 4: failed deadline has elapsed
attempt 5: failed deadline has elapsed
attempt 6: failed deadline has elapsed
attempt 7: failed deadline has elapsed
attempt 8: completed
attempt 9: failed deadline has elapsed
attempt 10: failed deadline has elapsed
pingora connector: ok=1 fail=9

raw attempt 1: completed
...
raw attempt 10: completed
raw h2 client: ok=10 fail=0
```

先前另一次 10 次运行得到 `ok=2 fail=8`，因此观察到的连接器成功率在 1/10 至 2/10 之间波动，而直接 `h2` 客户端每次 10/10。失败形态是 `read_response_header` 在 15 秒预算内不返回，或在更早的变体中出现 `BrokenPipe`/h2 `RST_STREAM`。

已知边界，避免过度归因：

- 两种客户端的差异不止一处（Pingora `Stream` 包装、`pingora_timeout` 计时器、连接池、独立驱动任务）。本阶段只证明“Pingora `Connector` 路径在真实 h2 服务端上不稳定”，**没有**定位到具体原因，也没有证明是 Pingora 的某一段代码缺陷。
- 运行环境是单机回环、当前工具链、`--offline`；未在其他主机或内核上复现。
- 一个临时最小工程（不在仓库内，未纳入版本控制）曾复现同一比例，但该工件已删除，不作为证据；可复现证据是本节的仓库内探针。

上述归因已被本节开头的复现与修复推翻。当前两个探针均纳入默认测试并断言全部成功；不存在据此认定的 Pingora h2 连接器阻断。

### 4.5 日志资源无副作用准备

`tests/feasibility/logging.rs` 用最小编排模型验证候选生命周期的四条不变量：

- `prepare` 不创建文件，目录保持为空。
- 第一个候选成功提交后第二个候选失败，回滚会删除已提交的候选，先前活动文件的 inode 与内容不变（`mid_activation_failure_rolls_back_every_candidate_and_keeps_the_previous_file`）。
- 被拒绝的候选不影响活动文件。
- 已提交 worker 持续写入直到 owner 释放。

这是对回滚编排语义的验证，不是文件系统事务证明：模型里的临时文件由测试自身创建和删除，没有覆盖“多个目标文件的 `rename` 序列在崩溃点中断”这一情形。CONTRACT.md 6 要求 009 用真实 `rename` 序列完成同样断言，未做到则按阻断处理。

## 5. 质量门禁

```sh
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo test --locked --offline --no-fail-fast -q -- --test-threads=2
```

修复前在 UID 0、`/root` 为 0700 的工作区，全量测试有四项权限用例失败（不是五项）：watch 需要非特权 UID，三个 static_files 用例的降权子进程不能遍历 `/root`。这不是产品行为修复点，未放宽工作区权限或修改用例以跳过权限断言。

修复后将 Cargo.toml、Cargo.lock、src、tests、docs、examples、README.md、writing-block.md 复制到独立 0755 临时目录 `/tmp/rill-001-fix-KHirnO`，以 root 离线构建，编译期可执行路径均指向该可访问目录。随后逐个用 `runuser -u nobody -- <测试可执行文件> --test-threads=2` 执行所有测试二进制。未使用原工作区 target 符号链接作为临时输出，未修改原工作区权限。

```sh
CARGO_TARGET_DIR="$validation/target" cargo test --manifest-path "$validation/Cargo.toml" \
    --locked --offline --no-run --message-format=json
cd "$validation"
runuser -u nobody -- "$test_executable" --test-threads=2
```

从 Cargo JSON 输出中选择 profile.test=true 且有 executable 的全部八个工件，逐一执行。实测结果：

```text
rill (main): 0 passed
rill (lib): 36 passed
configuration: 27 passed
feasibility: 25 passed; 0 ignored
http_proxy: 21 passed
https: 20 passed
reload: 28 passed; 1 ignored
static_files: 27 passed
```

合计 184 passed、0 failed、1 ignored；唯一忽略项为需要独立 DNS fixture 的 reload 测试，未启用。原先四项权限失败全部通过。本轮在原工作区实际执行 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo test --locked --offline --test feasibility -q -- --test-threads=2`、`cargo test --locked --offline --doc -q` 和 `cargo build --release --locked --offline -q`，全部通过（feasibility 25 项，doc 0 项）。独立 reviewer 复审修复 diff、原始复现日志与非特权全量日志，无新增发现；不代表 001 的缺失可行性证据已补齐。

## 6. 依赖

本阶段唯一构建变更：`Cargo.toml` 新增 `[dev-dependencies] h2 = "=0.4.20"` 与 `http = "=1.5.0"`，二者已在 `Cargo.lock` 中（作为 `pingora` 的传递依赖），新增后锁文件仅多出 rill 包自身的两条依赖名，无新包、无版本变化、无下载。`regex` 等其余依赖仍只是传递依赖，未在 001 直接引用；是否提升为直接依赖由各实现阶段按其用途单独记录（见 CONTRACT.md 第 8 节）。

## 7. 结论与未决项

已获得隔离证据：字面量跨块替换的分块无关性、真实 h2 上的 gRPC trailers、请求分块以独立 DATA 帧到达、取消不污染连接、受约束写入的原子提交与越界拒绝、锁与存储身份模型、日志候选无副作用准备与回滚编排。配置契约示例全部可按 KDL v2 解析。

后续[容器验收](014-caddy-grpc-container.md)已补上隔离 Pingora 原型的 h2c/TLS h2 四类调用、响应 trailers、取消与 deadline，以及错误 CA/名称拒绝，三轮 114 项判定通过。仍未获得的证据：正则捕获替换的分块无关性（只测了字面量）、同进程多协议/目标/信任池隔离、跨 Reload、请求 trailers、以及完整 DAV/日志资源交接。容器结果不代表生产 Rill CLI 功能已交付。

原 Pingora h2 连接器阻断已撤回：根因是测试后端未持续轮询 Connection；修复后 Pingora 与直接 h2 客户端各 100/100 次成功（4.4）。尚缺的验证证据仍阻止 001-T03、001-AC03 通过，不能由单个 h2c 一元探针推断完整 gRPC 可行。

未决：001-T04 用户对契约与限额的验收尚未进行。权限用例已在可访问的临时工作区以 UID 65534 执行通过，详见第 5 节。
