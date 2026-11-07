# 023：站点文件日志阶段验证记录

009 已完成独立复审、隔离验收及最终完整门禁。用户明确确认五秒仅限制用户态关闭等待，内核 D 态 IO 可能延迟进程回收。以下保留各轮历史失败与修复证据；未操作生产、获取网络工件或增加依赖。完成提交以包含本记录的 Git 提交为准。

## 实际执行

- `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`：通过。
- `cargo test --locked --offline -q --lib observability:: -- --test-threads=2`：13 项通过；包含文件身份、轮转、队列、候选回滚与 worker 生命周期。
- `cargo test --locked --offline -q --test http_proxy --test reload -- --test-threads=2`：root 下 HTTP proxy 44 项通过；reload 35 项通过、1 项按设计因 root UID 拒绝、1 项 DNS fixture ignored，因此该组合命令失败。
- 复制源码、测试、vendor 和示例到 `/var/tmp/rill009-suite-*` 0755 临时工作区，使用既有 `/var/tmp/rill001-closure-suite-bGidmR/target` 与 `--locked --offline --no-run --message-format=json` 构建，按 Cargo JSON 选择 http_proxy/reload 工件并通过 `runuser -u nobody -- <工件> --test-threads=2` 执行：HTTP proxy 44 项通过；reload 36 项通过、1 项 DNS fixture ignored。临时工作区已清理，未修改仓库 target 链接。上述全套在本轮最后数项细节修改前运行；之后的定向检查见下条。
- `cargo test --locked --offline -q --test http_proxy http_proxy::observability`：2 项通过，验证 check 不创建、拒绝禁止字段、JSON 配置身份、默认隐私、显式路径去查询、可信客户地址及 stderr 和站点文件并存。
- `cargo test --locked --offline -q --test reload file_log_`：2 项通过，验证失败 Reload 保持旧文件日志、成功切换及旧 SSE 完成时仍写入旧身份/旧 sink。
- 最后修改后重新复制源码至 0755 `/var/tmp/rill009-final-*`，离线构建 lib/http_proxy/reload，按同样方式以 nobody 定向重跑：observability lib 13 项、HTTP proxy 2 项、reload 2 项均通过；临时工作区已清理。

## 初轮缺口与后续证据

初轮未覆盖真实慢盘、轮转失败及重启恢复、资源饱和、关闭和部署；后续逐轮补齐情况见下文。手动建立轮转状态的早期测试不作为内核故障注入证据，后续实际服务限额故障单独记录。最终完整门禁与复审结论见末节；DNS ignored 未启用。用户已明确确认关闭边界，009 任务及验收已据最终结果勾选。

## 复审阻断修复（待再次复审）

独立复审指出退役磁盘目标未计量、低流量时未按天清理、短写残行三个 P1。本轮以进程寿命为边界，为每个候选/已发布目标保守预留 6 文件和 60 MiB，共最多 64 个目标；退役目标及无法证明清除的候选继续占信用，同目标安全复验后可重开。历史有界但不自动释放或删除管理员文件；重启不承诺清理，也不构成跨进程文件系统总配额。失败候选确认没有遗留文件时退券。

- `cargo test --locked --offline -q --test reload retired_log_disk_credit_bounds_sequential_reloads -- --test-threads=1 --nocapture`：真实服务连续发布 64 个不同目标，第 65 个返回 `LOG_CAPACITY`，旧服务仍响应，旧文件未自动删除；通过。首次运行发现控制面将该错误转换为 `INTERNAL`，修复协议映射后重跑通过。
- `cargo test --locked --offline -q --lib observability::log_file::tests::idle_worker_removes_expired_rotation_without_requests -- --test-threads=1`：真实文件时间戳和后台维护线程验证空闲过期回收；为缩短测试使用 20 ms 的维护周期，生产周期 60 秒；验证待恢复轮转项不会被清理。
- `cargo test --locked --offline -q --lib observability::log_file::tests::partial_write_recovery_refuses_foreign_file_and_retries`：验证残行恢复时拒绝替换的外来 inode，原身份恢复后截断残行再写；通过。
- 在 `unshare --mount --propagation private` 内创建专用 1 MiB tmpfs，运行 `RILL_LOG_ENOSPC_DIR=<私有挂载> cargo test --locked --offline -q --lib observability::log_file::tests::enospc_partial_record_is_rolled_back_before_next_record -- --ignored --exact --nocapture`：真实 tmpfs 填满、释放 4 KiB 后的直接 16 KiB 写探针返回短写；同等条件下日志写返回错误并恢复为零长度，释放空间后下一条独立 JSON 可解析；通过。挂载和目录已在私有命名空间清理。首次误将 ignored 用例直接运行而未设置 fixture，按预期失败并报 `isolated tmpfs required`，不计为通过。

仍需独立复审及父代理最终完整门禁；本记录不勾选 009 验收。空闲回收和短写的故障路径仍不代表外部同 UID/root 并发改写受到防护；该行为不在冻结配额保证内。

- 复审修复后的定向库测试以 nobody 运行：将本地离线构建的 lib 测试工件复制到 0755 临时目录后执行 `runuser -u nobody -- <工件> observability:: --test-threads=2`，17 项通过、隔离 tmpfs 用例按设计 ignored。真实权限回归使未发布候选目录不可写，确认删除失败时目标仍在且信用保留；恢复权限后可安全重开。临时文件已清理。
- 复审修复后重新执行 `cargo fmt --all --check`、`cargo check --locked --offline -q` 和 `cargo clippy --locked --offline --lib -q -- -D warnings` 通过；全目标及完整套件由父代理重新执行。

## 再次复审修复（待再次复审）

- 未恢复短写采用目标级进程寿命隔离标记；标记在写入前置位，仅完整写入或安全恢复后清除。旧 sink 退役及其他目标发布后，同名重开先检查标记，不能证明安全则拒绝激活；管理员须检查修复后重启。进程重启不保证自动识别或修复历史残行。
- `cargo test --locked --offline -q --lib observability::log_registry::tests::partial_write_quarantine_survives_retirement_and_same_target_reopen -- --test-threads=1`：构造真实残行及不可恢复标记、退役旧 sink 并发布其他目标后，同名重开拒绝且原文件不变；通过。该测试不冒称第二次实际 ENOSPC。
- 文件日志关闭超时时设置固定、有界的 `LOG_TIMEOUT` 原子诊断标记，由既有 stderr worker 尝试输出，关闭线程不等待磁盘。`cargo test --locked --offline -q --lib observability::access::tests::timeout_diagnostic_is_nonblocking_and_redacted -- --test-threads=1`：真实后台 stderr 路径输出 JSON 且调用非阻塞；通过。未实测真实服务卡住文件 writer 满五秒的场景，不能据此声称端到端关闭期限已通过。
- `cargo test --locked --offline -q --lib observability::log_directory::tests::fd_saturation_rejects_directory_prepare_without_side_effects -- --test-threads=1`：独立测试进程持有 256 个真实 FD 及对应信用，额外父目录准备返回 `LOG_CAPACITY` 且不创建目标，释放部分信用后恢复；通过。该测试验证信用边界，不冒称实际服务在 256 日志 FD 饱和下通过。
- 父代理完整非 root 套件记录 `access_log_policy_reloads_and_removed_exporters_are_rejected` 发生 ENOENT。对其提供的独立工作区和测试工件定向重跑 40 次均通过；源码检查确认 `Fixture::start` 原先只等待 TCP listener，而生产启动先绑定 TCP、后绑定控制 Socket，存在测试先连接 TCP 后立即 Reload 的 ENOENT 窗口。测试 fixture 现同时等待实际 Socket 类型路径；没有修改生产启动次序。无法证明该窗口是该次失败的唯一原因，完整套件仍须父代理复跑。
- 真实服务慢文件 writer/队列满、真实服务五秒关闭与部署约束仍未实测；不因库级测试推断其通过。009 验收保持未勾选。
- `cargo test --locked --offline -q --lib observability::log_registry::tests::saturated_queue_drops_without_waiting_for_real_file_worker -- --test-threads=1 --nocapture`：使用真实文件和 worker，测试线程暂持文件锁使 worker 停滞；1024 条入队后下一条立即丢弃并计数，解除锁后 worker 正常关闭。通过；这是库级受控锁竞争，不是实际服务慢盘验证。
- `cargo test --locked --offline -q --lib observability::log_registry::tests::blocked_file_worker_shutdown_emits_timeout_without_extending_budget -- --test-threads=1 --nocapture`：独立测试进程中，真实文件 worker 被测试线程持有的文件互斥锁阻塞，调用真实 `Observability::shutdown`；约 5.11 秒返回并从真实 stderr 后台路径输出 `LOG_TIMEOUT`，解除锁后清理。通过；这是库级受控锁竞争，不是实际服务慢盘或完整部署验证。
- 最后修改后的 lib 观测测试：`cargo test --locked --offline -q --lib observability:: -- --test-threads=2` 为 22 项通过、1 项专用 tmpfs 用例 ignored；将同一工件复制到 0755 临时目录后以 nobody 运行同一过滤条件，亦为 22 项通过、1 ignored。定向 `reload::transactions::access_log_policy_reloads_and_removed_exporters_are_rejected` root 环境通过。

## 最终复审后的关闭与隔离实测

父代理报告在 `/var/tmp/rill009-verified-dnbjhdv1` 的完整非 root 门禁：10 个测试工件合计 341 passed、4 ignored；其中 lib 96+1 ignored、configuration 37、feasibility 31、http 44、https 28、reload 37+1 ignored、static 27、body 22、webdav 19+2 ignored。父代理同时报告当时全目标 clippy 和 release 构建通过。这是本次关闭修复之前的父代理结果，不能当作修后重跑；修后仍须父代理复验。

针对 P2，关闭用户态等待的总预算仍为 5 秒，其中文件 worker 预算最多 4.75 秒，余量留给 stderr worker 处理和退出确认；stderr 阻塞不再获得额外等待。`cargo test --locked --offline -q --lib observability::log_registry::tests::blocked_file_worker_shutdown_emits_timeout_without_extending_budget -- --test-threads=1 --nocapture` 的独立子进程在 `shutdown` 返回和时限断言后立即调用 `process::exit(0)`，不解锁、不 sleep 或执行清理；父进程在其退出后实际读到 `LOG_TIMEOUT` 并清理本次目录，约 4.76 秒通过。`blocked_stderr_does_not_extend_shutdown_budget` 使用真实填满的非阻塞 pipe 后恢复阻塞模式，子进程约 5.02 秒退出，通过；阻塞 stderr 时不保证诊断送达。

### 真实服务：冻结文件系统、队列与关闭

先在私有 mount namespace 中仅对本次新建的 16 MiB 镜像执行 ext4 格式化、申请空闲 loop、核对 backing file、挂载和解除，实际输出 `PROBE_MOUNT_OK`、`PROBE_CLEANUP_OK`。随后在新的私有 mount/net namespace 内新建 32 MiB ext4 镜像，反复核对 loop backing file 和挂载来源后，仅冻结本次挂载，运行本次构建的真实 rill 与 loopback HTTP 客户端。队列场景的原始关键输出：

```text
REQUESTS count=1 elapsed=0.003s max=0.003s
FREEZE_QUEUE_OK
REQUESTS count=1150 elapsed=0.384s max=0.002s
THAW_QUEUE_OK
QUEUE_LOG_LINES=1025
LOG_DROPPED_SEEN
```

1150 次请求均返回 200；解冻后日志共 1025 行（包括冻结前一条），并观察到 `LOG_DROPPED`。这是本次隔离环境的实际结果，不是吞吐承诺。

同一私有 ext4 另起真实服务，冻结后请求返回 200，再发 SIGTERM。原始关键输出为 `FREEZE_SHUTDOWN_OK`、`REQUESTS count=1 elapsed=0.002s max=0.002s`、`SHUTDOWN elapsed=4.748s`；后者只表示领头线程进入 zombie 状态，**不是整个进程已回收**。实际 stderr 含 `{"time_ms":1794022741399,"level":"warn","event":"LOG_TIMEOUT"}`；阻塞 ext4 写入的 worker 处于内核 D 态，外层等待超过 120 秒被工具终止。随后通过仍存活线程的私有 mount namespace 核对 loop1 backing file 和挂载来源，解冻本次文件系统，确认线程消失、仅解除该 loop 并删除本次目录。没有冻结既有宿主盘。此结果不能称真实进程五秒退出；随后用户明确回复“五秒限制用户态关闭等待；内核 D 态 IO 可能延迟进程回收”，据此同步契约、配置、架构及验收，不把本次结果冒称整个进程五秒内回收。

### 真实服务：轮转失败、恢复和重启

在另一独立 mount/net namespace 的自建 32 MiB ext4/loop 镜像中，以 16372 字节既存有效 JSON 文件接近轮转阈值；仅将本次 rill 进程的 RLIMIT_NOFILE 软限额临时降至 16、保留 hard limit 524288，持久 HTTP 连接仍返回 200。首次探测发现 FD 表有空洞，未触发故障即退出并清理；第二次根据实际空洞选择软限额，观察活动文件已 rename 为 `.1`、新活动文件缺失及 `LOG_DROPPED`。恢复本进程软限额后第二请求仍 200，活动文件恢复；优雅退出并重启后再请求 200，原始验证输出 `VALID_JSON access.jsonl lines 2`、`VALID_JSON access.jsonl.1 lines 1`、`ROTATION_FAILURE_RECOVERY_RESTART_OK`。该测试只改本次子进程限额和本次文件系统，loop 和目录已清理。

### Podman 部署约束

使用本地已存在镜像 `localhost/rill:bench`（ID `ff91a3400dae`，未拉取）及只读 bind 的本次 debug 二进制，创建唯一临时容器：rootful、容器 UID/GID 10001、`--network=none`、只读根、全部 capability 丢弃、no-new-privileges；管理员预创建并以 UID 10001、0700 权限 bind 运行目录和日志目录。容器内 check 通过，从容器网络命名空间经 loopback 请求得 200，文件 JSON 中 site 为 `container`、route 为 `health` 且无默认 path；控制 Socket Reload 返回 generation 2，容器正常停止并删除。另建唯一临时容器将同一新建日志目录以只读方式挂载，run 退出 1、诊断 `RESOURCE_IO`，且未创建日志文件。只删除本次容器和临时目录，未拉取镜像或运行全局 prune；未覆盖 rootless 和 systemd 实际部署。

修后补跑：`cargo test --locked --offline -q --lib observability:: -- --test-threads=2` 为 23 passed、1 专用 tmpfs ignored；HTTP 文件日志和 Reload 定向各 2 项通过。将最终 lib 工件复制到 0755 临时目录，以 nobody 定向运行上述两个 `blocked_` 关闭用例，2 项通过（合计 9.77 秒），临时目录已清理。`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings` 均通过。最终完整门禁结果见下节。

## 父代理最终门禁与复审

最后关闭修复后，父代理执行 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo build --release --locked --offline -q` 和 `git diff --check`，全部通过；实际工具链为 rustc 1.99.1，不声称 MSRV 构建。

将完整仓库内容（排除 `.git`、`.pi` 和 `target`）复制至 0755 `/var/tmp/rill009-finalgate-4rn41_7i`，使用既有外部缓存和 `cargo test --locked --offline --no-run --message-format=json` 构建，按 Cargo JSON 选择全部测试工件，逐一 `runuser -u nobody -- <工件> --test-threads=2`：10 工件全部通过，合计 342 passed、4 ignored。分别为 lib 97+1 ignored、configuration 37、feasibility 31、http_proxy 44、https 28、reload 37+1 ignored、static_files 27、body_transform 22、webdav 19+2 ignored，main 无测试。默认 ignored 包括 DNS、专用 tmpfs、DAV 客户端及 DAV 磁盘场景；日志 tmpfs 用例已在上述独立专用环境另行通过，不批量启用 ignored。各工件原始输出保存在该临时目录的 `gate-*.log`。

曾因初次复制遗漏 docs/原文资源而导致 feasibility 构建失败；补全后的一轮完整测试遇到 Reload ENOENT，修复测试启动就绪窗口后连续两轮完整套件通过。失败记录不算成功证据。未替换或删除仓库 target 链接。

独立 reviewer 经过多轮只读复审，最终结论为 `No issues found`，已解除三项 P1 和关闭诊断 P2；确认现有隔离证据在用户批准 D 态关闭边界、文档同步且最终门禁通过的条件下足够。本轮复审不冒称 reviewer 独立运行测试。用户已明确确认关闭边界，相应契约及文档已同步，完成条件满足。未覆盖 rootless/systemd 实际部署，也不声称验证所有文件系统、内核故障或跨重启磁盘总配额。
