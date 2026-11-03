# 009 Snippet 与可选自动 Reload：验证记录

009 验收通过：最终质量门禁、release、示例实际运行及独立交付复审均已完成。阶段状态见子计划，独立完成提交见 Git 历史，未开始 010/011。基线 HEAD 为 `2059f1`。访谈 `fb950ddd` 批准使用既有 nix 0.31.3 的 inotify feature，以及 watch 启动开关和有界预算语义；Cargo.lock 未新增包，最低 Rust 版本仍为 1.95。原始 `writing-block.md` 不变。

## 证据环境与范围

父代理提供的隔离包装为 `run-quality.sh`、`run-check.sh`，临时工作区 `/var/tmp/rill-009.L2km31`；source 只读，Rust 1.99.1，registry 只读离线。运行身份 UID 65534、GID 0，清空附加组和 capabilities；私有 PID、mount、network namespace，网络仅 loopback，低端口 UDP 53 仅在 netns 内开放。resolv 配置 timeout 30、attempts 2、ndots 1；jobs、link threads、test threads 均为 2，CPU affinity 6、7。scratch 4 GiB，AS 2 GiB，CPU 300 秒，nproc 64，nofile 256，fsize 512 MiB，wall 600 秒后 10 秒强杀。CLI/control 日志仅本地保存，不公开。父代理的外部日志为 `artifacts/009-*-check.log`；具体运行与最终产物须由父代理核对归档。

最终冻结版本 `source-final` 的完整 131 项通过：core 18、configuration 23、feasibility 7、HTTP 19、HTTPS 20、reload 26、static 18。实际命令为 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo test --locked --offline --no-fail-fast -q -- --include-ignored --test-threads=2 --nocapture`，组合退出码 0。真实 DNS ignored 用例仅在上述隔离 resolver 执行；reload 组 32.30 秒通过。日志为 `/var/tmp/rill-009.L2km31/artifacts/009-final-quality.log`。

## T01：Snippet

- 已做：完整路由展开、嵌套引用、独立名称空间、全部定义校验、来源链和全配置预算。独立 review 发现并修订来源链、成本去重及步骤测试等问题。
- 可核对测试：`snippet_nested_forward_reuse_keeps_paths_and_profile_precedence`、`snippet_invalid_definitions_and_conflicts_are_rejected`、`snippet_expansion_limits_are_enforced`、`snippet_reference_errors_keep_cross_file_call_chains`、`snippet_conflict_deduplicates_deep_import_and_reference_chains`、`snippet_payload_budget_deduplicates_imports_and_reports_nested_chain`（`tests/configuration.rs`）。
- 结果：最终配置集成及 HTTP 两站点实际复用通过；新示例另由 release 验证。

## T02：依赖追踪与 native watch

- 已做：入口、导入、glob、证书及私钥依赖追踪；inotify 注册及有界资源管理。review 闭合实际消费路径 FD 与 symlink 绑定、ENOTDIR 恢复、非 UTF-8 glob 和目录预算等问题。
- 可核对测试：`watch_startup_mode_and_check_do_not_allocate_inotify`、`watch_symlink_atomic_import_and_glob_graph`、`watch_non_utf8_glob_recovers_and_ignores_unmatched_file`、`watch_resources_detect_registration_gap_and_failure_baseline`（`tests/reload/watch.rs`）；`watch_configuration_is_boolean_and_singleton`、`tracked_build_retains_glob_and_symlink_dependencies_on_failure`（`tests/configuration.rs`）。
- 结果：`watch_resources_reject_consumed_symlink_target_change` 验证原始消费路径与 FD 绑定；`watch_resources_reports_real_inotify_queue_overflow` 使用真实内核队列验证 `WATCH_OVERFLOW` 和重建状态，不以普通 drain 预算耗尽代替 overflow。启动关闭、图变化、注册间隙、权限恢复均通过。最多两个 FD 由资源所有权和 execution permit 限制，并有 TLS 长连接测试的峰值采样与静止收敛检查；采样不能证明任意时刻的全时序上限。

## T03：调度与失败保留

- 已做：与手动 Reload 共享候选准备和提交路径，有界去抖与重试；review 修订失败 baseline 注册间隙、先 swap 后 drain 旧事件，以及 unavailable recovery 的 1 秒退避。稳定普通配置错误等待后续事件；native 操作不可用时按 1 秒有界重试。活动与最新失败部分图合并超预算或无法覆盖时进入 degraded，而非无限保留历史。
- 可核对测试：`watch_imported_profile_changes_real_timeout`、`watch_failed_imports_recover_without_entry_edits`、`watch_burst_concurrent_cli_converges`（`tests/reload/watch.rs`）。
- 结果：`tests/https/reload.rs::watch_tls_websocket_survives_reloads_and_certificate_failure_without_fd_leak`通过六轮自动更新、证书轮换、key 不匹配失败回滚和自动修复；旧流持续双向交换，新握手与路由使用新状态，正常关闭及 Socket 清理通过。`watch_permission_denied_retries_are_bounded_and_chmod_recovers` 验证拒绝期间既发生重试又不高频重试，恢复权限后自动切换；最终 burst 测试在 CLI 全部结束后另写独有 gamma 版本，必须由 watch 完成收敛，不能由 CLI 代偿。50 ms drain 消费 inotify 内核事件队列，不是文件轮询；idle 开销未测，不宣称为零。

## T04：示例、文档和最终验证

- 已做：复用示例、独立显式 watch 示例与配置/架构文档；不引入后端或证书。父代理在只读源副本运行统一格式检查与完整质量门禁。
- 结果：`cargo build --release --locked --offline -q` 成功，独立执行子代理在隔离环境验证两个示例的 check、双站点共享及独立路由、默认 0 / 开启 1 个 inotify FD、原子替换 Snippet 后两站点自动更新、正常退出 0 和 Socket 清理。日志为 `artifacts/009-release-examples.log`，组合退出码 0。release 构建使用相同只读源和工具链、scratch 8 GiB、AS 3 GiB、CPU 900 秒、fsize 1 GiB、wall 1800 秒，CPU affinity 8,9。
- 产物：`/var/tmp/rill-009.L2km31/artifacts/rill`，19,054,072 字节，SHA-256 `06cf6d52a48e8f6450e8682e3d2359f0417b8a03c762b10c9932db367353dcf8`。未将二进制或测试私钥纳入 Git。独立最终交付复审未发现 blocking/high/medium 问题；任务与 AC 已按实测证据更新。小型日志和源码指纹收录在 [data/009/](data/009/)，原始执行证据保留在外部 artifacts。

## 已观察问题和边界

早期依赖追踪对祖先目录 mtime 过敏，使原 P0 测试出现 `RESOURCE_CHANGED` 误失败；已改为普通 build 禁用 tracking、目录比较 identity 而非无关 mtime，后续完整门禁通过。一次旧中间 snapshot 的静态测试 `directory_index_symlink...` 返回空结果，原测试未改；后续完整 119、126、127 项门禁及同用例五次独立重复均通过。初始失败无法稳定复现，根因未知，不声称已修复某个产品问题，最终 131 项门禁及后续重复检查未复现；保留初次失败日志，不把未知根因表述为已修复。

只读 review 不等于全面安全审计或渗透测试；不承诺恶意文件系统的跨文件事务。监听依赖 Linux 可信本地文件系统及既有 proc/namespace 信任边界，远端文件系统或挂载变化的事件不保证。性能、rootless 及 008 第三目标对照不属于本阶段验证。
