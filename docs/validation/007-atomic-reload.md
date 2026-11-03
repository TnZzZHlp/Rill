# 007 原子 Reload 验证记录

## 状态与范围

用户授权 SubAgent 实施、复审、测试通过后独立提交。007 实现及 106 项检查通过，最终新上下文独立只读复审 No issues found、Merge verdict OK；先前两次只读预审 OK 不替代最终复审。独立提交由父代理核对后执行。005 提交 `2b9fe02e4dd84b82961f17db939495fb0211d589`，006 提交 `2ba06369cfda291b53d968c1319fcbf507dcff7d`；008–012 未开始。

用户访谈批准直接使用已有 serde 1.0.229 derive、serde_json 1.0.154 及其本地缓存新锁包 zmij 1.0.23、nix 0.31.3 user 功能；无其他依赖升级、新工具或测试框架。

## 实际隔离门禁

父代理在 src/tests/Cargo 完全相同的只读副本执行：可信外层 `unshare --net` 准备 loopback，并仅在该 netns 设置 `ip_unprivileged_port_start=0`；目标 bwrap `--share-net` 只共享此隔离网络，绝非宿主网络，同时隔离 PID 和 mount。UID 65534、GID 0，清空补充组，无 capability。`/etc/hosts` 只有 `127.0.0.1 localhost` 和 `::1 localhost`；resolv 使用 `nameserver 127.0.0.1`、`options timeout:30 attempts:2 ndots:1`。toolchain、registry、source 只读，scratch 4 GiB；AS 2 GiB、CPU 300 秒、nproc 64、nofile 256、fsize 512 MiB，总墙钟 600 秒加 10 秒 kill。Rust/Cargo 1.99.1，vendored OpenSSL 3.6.3。

清空环境后白名单同 006：`PATH=/opt/rust/bin:/usr/bin:/bin HOME=/scratch/home TMPDIR=/scratch/tmp CARGO_HOME=/scratch/cargo CARGO_TARGET_DIR=/scratch/target CARGO_BUILD_JOBS=2 CARGO_NET_OFFLINE=true CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS=-C link-arg=-Wl,--threads=2 LANG=C.UTF-8 LC_ALL=C.UTF-8`，测试线程 2。

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --check` | 通过 |
| `cargo check --locked --offline -q` | 通过 |
| `cargo clippy --locked --offline --all-targets -q -- -D warnings` | 通过 |
| `cargo test --locked --offline --no-fail-fast -q -- --include-ignored --test-threads=2 --nocapture` | 106 项通过，0 失败、0 ignored：17 核心、15 配置、7 可行性、18 HTTP、18 HTTPS、14 Reload、17 静态。默认 ignore 的 DNS 测试本轮实际运行。Reload 二进制 30.21 秒，HTTPS 18 项 10.36 秒；旧 79 项回归通过。 |

## 任务和验收证据

- T01 / AC01：统一 build 准备完整 TLS、static、upstream 并构造 router/logger，唯一 ArcSwap store 发布；`failed_reload_preserves_state_and_generation` 检查非法配置、缺失 root、Profile 引用及 HTTP/control 重启项失败不推进。`reload_follows_retargeted_entry_symlink` 验证原始绝对入口链接可切目标。`changed_http_address_requires_restart`、`changed_control_socket_requires_restart`、`generation_overflow_rejects_publish`、`expired_deadline_rejects_publish` 验证重启、溢出和截止拒绝。
- T02 / AC02：`malformed_frames_do_not_publish_and_valid_peer_can_reload`、`socket_bind_rejects_existing_entries_and_unsafe_parents`、`replacement_file_survives_server_shutdown` 覆盖畸形帧、权限、既存项和替换后的关闭清理；`only_service_uid_and_root_are_authorized` 覆盖核心授权策略；`binds_long_valid_unix_path` 覆盖合法长路径。`concurrent_reload_generations_are_contiguous_and_snapshots_complete` 使用约 900 KiB 真实准备和 8 次调用，成功代次连续唯一、允许 BUSY，header/body 属同一快照。
- T03 / AC03：`reload_updates_keepalive_snapshot_and_cli`、`cli_exit_codes_for_invalid_arguments_and_missing_instance` 及失败场景验证真实结果、退出码和代次；CLI 不重试，不把发送视为成功。
- T04 / AC04：`tls_reload_changes_new_handshakes_and_keepalive_routes_and_rolls_back_invalid_cert` 验证新握手新指纹、旧 TLS 身份/证书不变而请求 root/cache 更新，坏证书 TLS_INVALID 保留旧代次、恢复后 gen3。`http2_existing_connection_uses_reloaded_routes_for_new_streams` 验证同 fd 新流 root 切换、删除域名 404、alias 421。`websocket_survives_proxy_reload`、`sse_chunks_survive_proxy_reload` 验证旧流持续、新请求使用新后端；`large_file_keeps_old_root_during_reload` 验证 64 MiB 旧 root 流跨 reload 完成旧字节、新请求用新 root/header。并未新增 007 SIGTERM 大文件测试，旧 005 SIGTERM 回归仍通过。`changed_imported_profile_updates_proxy_timeout` 仅改导入 Profile，30ms 返回 504、500ms 返回 200；`removed_route_returns_404_while_other_route_remains` 覆盖删除路由。
- `timed_out_prepare_keeps_admission_and_never_publishes_late` 默认 ignore 以保护宿主 53 端口，本轮在隔离网络真实运行。本地 UDP DNS 延迟 A/AAAA 令真实 prepare 超过 30 秒，首个 CLI 返回 PREPARE_TIMEOUT、退出 1，证明等待首字节未被 5 秒截断；真实任务仍阻塞时新 CLI 仍 BUSY。放行后迟到有效候选被弃，唯一排队任务成功 gen2 而非 gen3，后续合法任务 gen3。不是假 builder，也未使用 mock 框架。
- `publish_replaces_snapshot_and_close_rejects_candidate`、`queued_reload_is_bounded_and_close_cancels_waiter` 和弱引用释放相关核心检查覆盖关闭、排队与旧快照释放。关闭与发布共享短 gate，最终检查 closed/deadline；blocking closure 只构建，两个 OwnedPermit 由异步提交和真实 blocking closure 共同持有，取消等待不能提前释放真实任务额度。

## 过程修正与边界

长路径 unit 最初硬编码 `/tmp`，沙箱中 `/tmp` 指向 scratch 的 symlink 被安全策略拒绝，改用 `TMPDIR` 真实目录。TLS 轮换测试最初用两个相同 subject 的自签叶作为 trust anchors 导致 OpenSSL 锚歧义，改用同一临时 CA 签发的两个叶证书，未改变产品验证策略。前置复审 P2 长名称/proc 路径长度问题改为短临时名加 rename NOREPLACE；父代理发现客户端读帧 5 秒提前终止合法 prepare 等待，修为等待首字节后才计帧期限，并由真实 30 秒测试验证。移除 unused import、整理格式，未关闭 lint。提交前补查还发现旧代静态长流存在时，逐候选新建的 128 许可池会跨 Reload 累计放大额度；已改为进程级 OnceLock 共享许可池，不持有快照或 root，并由独立复审确认。新增 prepared_generations_share_static_io_limit 实测旧代占满时新代无法取得、释放后可取得；再次全量门禁 106 项通过。

Socket 父目录 0700、Socket 0600 且 owner 为服务 euid，仍不能替代 SO_PEERCRED。逐目录 FD 校验及安全内核/proc 能力不可用时 fail closed；既存项保留，不自动清遗留。Drop 的身份检查与 unlink 非原子，不保证对抗同 UID/root 恶意写入；bind 后首次 inode 检查失败可能保守留下临时项。

未动态覆盖不同 UID wire 伪造授权：隔离环境单 UID，仅实测同 UID 与核心 policy 拒绝。恶意 FUSE 或不可中断文件 I/O 的强制杀停不承诺；prepare 期间外部跨文件及入口 symlink 变化不提供文件系统事务。超时/关闭无晚 store 有 state gate 逻辑及真实超时实测，不声称所有外部 I/O 可强杀。未进行独立全代码库安全审计、release build、部署或性能测试，后者属于 008。
