# 005 静态文件验证记录

## 状态与范围

005 实现及 69 项验证通过，最终独立只读复审结论 No issues found、Merge verdict OK，无未关闭意见；独立完成提交由父代理执行。用户明确授权 SubAgent 实施、复审、测试及通过后提交；最终复审者未自行运行测试。006/007 未实施，未执行 release build、独立安全审计或生产性能基准。

| 证据 | 实际结果及限制 |
| --- | --- |
| `tests/static_files/security.rs` | 编码、根内链接及越界拒绝、root 替换、链接切换、目录索引约束、FIFO/socket 和拒绝访问不 SPA 回退；结果在可信本地文件系统及 procfs/FD 边界内，不等于消除所有文件系统竞态。 |
| `tests/static_files/responses.rs` | 方法、元数据和条件请求；单 Range、If-Range 日期；SPA 导航限制。HEAD 忽略 Range；非法/多段 Range 返回完整 200。日期验证器秒精度，同秒更新有局限。 |
| `tests/static_files/lifecycle.rs` | check 检查 root 不绑定或修改资源，拒绝不存在、非目录及不可读 root；静态与代理完整路径；短预算本地拒绝；大文件排空及截断。check 不验证 DNS、连接或上游 TLS 信任。 |
| `src/static_files/range.rs` 纯函数回归 | 超大十进制安全饱和并检查原始逆序，不因溢出将有效范围忽略。 |
| 状态机回归 | 精确过期返回 504；真实短 deadline Upgrade 只能产生一份 501 或 504。 |

安全打开以 O_PATH root 和 openat2 BENEATH/NO_MAGICLINKS 探测为基础，并验证 procfs 与自有 FD；客户路径先 O_PATH 检查类型，再经可信 procfd 重开并比对身份，原 FD 持存。索引及 SPA 从原 root 重开。相对根内符号链接受约束允许，绝对、越界、magic link 拒绝，不支持时 fail closed。不保证恶意挂载、FUSE、NFS 不阻塞，也不替代 OS hardlink 和内容发布权限。

全静态共享 128 许可，Opened 和 blocking 任务持有许可；max blocking 16，单块至多 64 KiB 并逐块背压。最终响应头前交接预算和 response_started；正常关闭保留 30 秒排空，shutdown_background 避免未完成阻塞 IO 卡住运行时析构。许可耗尽排队取消、阻塞 IO 取消、HTTP/2 静态流取消未逐项动态验证，仅代码审阅。

## 实际隔离门禁

父代理在与 src/tests/Cargo 完全同一源码的只读副本运行；bwrap 独立网络（仅 loopback）、PID 和 mount，UID 65534、GID 0（只读既有注册表组权限），无 capability、补充组清空。toolchain/source/registry 只读，仅 4 GiB scratch 可写。每进程 AS 2 GiB、CPU 300 秒、nproc 64、nofile 256、fsize 512 MiB；整体限时 600 秒再给 10 秒 kill。Cargo jobs 2、linker threads 2、测试 threads 2。Rust/Cargo 1.99.1，vendored OpenSSL 3.6.3。

清空环境后白名单：`PATH=/opt/rust/bin:/usr/bin:/bin HOME=/scratch/home TMPDIR=/scratch/tmp CARGO_HOME=/scratch/cargo CARGO_TARGET_DIR=/scratch/target CARGO_BUILD_JOBS=2 CARGO_NET_OFFLINE=true CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 RUSTFLAGS=-C link-arg=-Wl,--threads=2 LANG=C.UTF-8 LC_ALL=C.UTF-8`。

| 命令 | 结果 |
| --- | --- |
| `cargo fmt --check` | 通过 |
| `cargo check --locked --offline -q` | 通过 |
| `cargo clippy --locked --offline --all-targets -q -- -D warnings` | 通过 |
| `cargo test --locked --offline -q -- --test-threads=2 --nocapture` | 通过：69 项、0 失败；4 纯函数、15 配置、7 可行性、13 HTTP、13 HTTPS、17 static。旧 004 验证仍通过。 |
| `git diff --check` | 父代理通过；本文编辑后另行检查。 |

大文件测试为 64 MiB，客户端使用 64 KiB buffer，停读 150 ms 超过 request 50 ms，SIGTERM 后读取完整且进程退出 0；RSS/HWM 从 17980 KiB 到暂停时 19572 KiB，增加 1592 KiB，仅为单环境观测，不是性能承诺。截断测试在不足声明长度后 EOF/Reset，收到的字节均为 0x00，无 HTTP 错误拼接；之后小文件仍 200。

## 过程与依赖

用户批准直接依赖 nix 0.31.3（fs）和 httpdate 1.0.3；Tokio fs 获批但实现未使用，未开启。Cargo.lock 仅 rill 依赖列表变化，无升级、新包或测试框架。首轮实现 worker 无代码交付，随后分 Root、HTTP 决策、运行时和测试完成。只读复审发现超大 Range 溢出误忽略及 proxy 本地拒绝预算可能在写头中途取消并拼接 504；修复后由原 reviewer 复核关闭。

测试过程中 Clippy 曾报告零权限八进制格式，已改为 `0o000`；新增真实过期测试曾误用配置不支持的 1ns 而启动拒绝，属于测试输入错误，最终改用支持的 1ms。不能将 1ns 描述为动态通过。
