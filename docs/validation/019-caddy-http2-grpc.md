# 019：HTTP/2 上游与 gRPC 验证记录

本记录保留 005 各切片的实施、失败与修复历史；最终独立复审及父会话验收见末节。005-T01–T03、AC01–AC03 已完成，完成提交以包含本记录的 Git 提交为准。以下历史阶段的“未完成”不代表最终状态。

已从本地缓存提取 pingora-core 0.9.0，来源 SHA-256 为 `7fd4d1ddc6d6dbc1a97164ce13c301980c25c35422aab87b512c4c8b5123729b`。补丁在新 transport 完成后、返回连接之前调用可失败的出生钩子；HTTP/1、h2c 和 HTTPS 请求均使用该钩子；TLS h2 禁止 ALPN 降级。移除上游示例私钥与上游独立 lockfile，并将 zstd 约束固定到本项目原锁定版本，未升级依赖。registry 覆盖 token、弱引用回收与 fd 重用；完整连接选项、候选生命周期及强制碰撞跨协议隔离仍需独立审查。

`openssl-probe =0.1.6` 已作为直接依赖复用锁版 Pingora 的安全路径探测（MIT OR Apache-2.0，原锁校验和 `d05e27ee213611ffe7d6348b942e8f942b37114c00cc03cec254295a4a17852e`）。候选读取 bundle 和有序 hash 目录，密封匿名 memfd 校验 bundle，运行时使用私有目录和每 peer 验证 store；生命周期与配额实现仍需审查。锁版探测先将整个环境值作为路径检查存在性；探测结果存在时替换对应值，否则保留原环境值，真正未设置时才使用链接库 `OPENSSLDIR` 默认值。因而普通冒号列表可能在 Pingora 初始化时被系统探测值覆盖，不能错误宣称原值始终优先。OpenSSL 3.6.3 的 `include/internal/common.h` 将默认路径定义为 OPENSSLDIR 加 `/cert.pem` 和 `/certs`；`openssl::version::dir()` 暴露链接库构建值。

执行结果：

- `cargo fmt --all --check`：通过。
- `cargo check --locked --offline -q`：通过。
- `cargo clippy --locked --offline --all-targets -q -- -D warnings`：通过。
- `cargo test --locked --offline -q --lib proxy::pool_policy::tests`：2 项通过。
- `cargo test --locked --offline -q --lib proxy::upstream_tls::tests`：通过；隔离子进程覆盖空值、不存在路径及冒号列表，并与锁版 probe 结果对照。
- `cargo test --locked --offline --test http_proxy h2c_ -- --nocapture`：2 项通过，真实 rill run 验证 h2c scheme、authority、响应及 HTTP/1 gRPC 明确拒绝、上游未收到业务请求。第一次边界测试失败：Pingora 在上游过滤前将克隆请求改为 HTTP/2，改用原始下游版本后通过。
- `cargo test --locked --offline --test configuration -q -- --test-threads=2`：36 项通过。
- `cargo test --locked --offline --test http_proxy -q -- --test-threads=2`：39 项通过。
- `cargo test --locked --offline --test https -q -- --test-threads=2`：23 项通过。
- `cargo build --release --locked --offline -q`：通过。
- `cargo test --locked --offline --test https upstream_trust_is_frozen_until_reload -- --nocapture`：通过，修改源 CA 后原快照仍验证原上游。
- `cargo test --locked --offline --test https upstream_hash_directory_symlink_is_frozen -- --nocapture`：通过，绝对 hash 符号链接目标删除后，原快照仍使用私有副本验证。
- `cargo test --locked --offline --test https tls_h2_refuses_missing_alpn_before_business_bytes -- --nocapture`：通过，502 且后端无业务字节。
- 隔离 Podman 缓存 debian image、真实 rill run 到 tonic TLS h2：HTTP GET 获得 200；容器 ID `5552536d7ba40a8de782ee0d1e508dadf9cd18e7cd261857d38fa1167fe7d26d` 和网络 `podman2` 均已删除，私有临时目录已删除。此测试不构成 gRPC 验收。
- `git diff --check`：通过。

用户已批准系统 CA 冻结资源语义及预算和 4096 出生记录额度，相应实现尚需独立安全审查与完整边界验证；真实 Rill gRPC、强制碰撞、跨 reload、TLS 与完整阶段质量门禁均未执行。没有修改计划验收勾选。

用户已批准每上游 h2 连接 16 个并发流，后端 SETTINGS 可进一步限制；实际同连接并发和取消隔离尚待验证。

全套 HTTPS 首次运行出现一次 WebSocket EOF；单项重跑和随后完整重跑通过，原因尚未确定。生产 release 构建已通过；CA 变动竞争、坏项、CRL、强制碰撞和连接复用生命周期仍需独立审查及补充测试。

## 真实 Rill gRPC 初轮（非完整验收）

运行 `cargo build --release --locked --offline -q`，然后 `python3 tests/feasibility/grpc_container/run.py --rill`。客户端通过真实 Rill TLS 入站连接，分别代理 h2c 和 TLS h2 上游；backend 为隔离容器中既有 tonic 服务，不是 001 原型代理。首次因客户端 URI authority 为 IP 而 SNI 为 `grpc.test`，Rill 按既有 SNI/Host 边界返回 421；设置测试客户端 origin 为 `grpc.test` 后重跑。另一次成功运行因客户端与服务端日志同名被覆盖，修正文件名后以下结果来自完整重跑：

```text
PASS https://127.0.0.1:49245 unary
PASS https://127.0.0.1:49245 512KiB flow-control
PASS https://127.0.0.1:49245 client-stream
PASS https://127.0.0.1:49245 server-stream and success trailers
PASS https://127.0.0.1:49245 grpc-status=15 and grpc-message after DATA
PASS https://127.0.0.1:49245 bidi responses before request EOF
PASS https://127.0.0.1:49245 trailers-only grpc-status=3 and grpc-message
PASS https://127.0.0.1:49245 cancel reaches backend and sibling RPC succeeds
PASS https://127.0.0.1:49245 deadline cancels backend and sibling RPC succeeds
PASS https://127.0.0.1:60147 unary
PASS https://127.0.0.1:60147 512KiB flow-control
PASS https://127.0.0.1:60147 client-stream
PASS https://127.0.0.1:60147 server-stream and success trailers
PASS https://127.0.0.1:60147 grpc-status=15 and grpc-message after DATA
PASS https://127.0.0.1:60147 bidi responses before request EOF
PASS https://127.0.0.1:60147 trailers-only grpc-status=3 and grpc-message
PASS https://127.0.0.1:60147 cancel reaches backend and sibling RPC succeeds
PASS https://127.0.0.1:60147 deadline cancels backend and sibling RPC succeeds
```

本轮 image `394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853`；容器 `6755c5f7118d8a87ef1cdff51a45f2f1ed6f154b3cd410ef1c405910a1f6cc63`、网络 `rill-grpc-acceptance-7nmksc4u` 和私有临时目录已清理。输出仅证明取消后另一次 RPC 成功，尚未证明兄弟流属于同一 TCP 连接；DATA 独立帧、TLS 故障计数、强制碰撞、Reload、重放与背压边界仍未完成。未声称通用请求 trailers 支持。

## 后续真实连接与审查修正

再次运行 `python3 tests/feasibility/grpc_container/run.py --rill`，以 backend 接收连接的对端 SocketAddr 确认取消流存活时的并发 unary 属于同一上游 TCP，取消后该连接仍可处理 unary；deadline 前后也核对同一上游 TCP。h2c 旧 bidi 流在 reload 后继续收发，新 unary 到另一后端：

```text
PASS https://127.0.0.1:44177 cancel reaches backend and same-TCP sibling RPC succeeds
PASS https://127.0.0.1:44177 deadline cancels backend and sibling RPC succeeds
id=298550-1793893800799491891-0 generation=2
PASS old bidi survives reload; new RPC backend=two
PASS https://127.0.0.1:53119 cancel reaches backend and same-TCP sibling RPC succeeds
PASS https://127.0.0.1:53119 deadline cancels backend and sibling RPC succeeds
```

容器 `eb7851e755fca1341c684cd0cbd5ccd9988d0c9478a30c67b4d2634fa9f947b4`、网络 `rill-grpc-acceptance-v3_69768` 和临时目录清理完成。之后安全目录修正后又通过一轮，容器 `6cb8fcca22211f7bd583dd8011a7ca91dab39005ba74797913daa338656cad6e`、网络 `rill-grpc-acceptance-a6pkodf0` 和临时目录均清理。独立工具 `cargo fmt --manifest-path tests/feasibility/grpc_container/Cargo.toml --check`、`cargo clippy --manifest-path tests/feasibility/grpc_container/Cargo.toml --locked --offline --all-targets -q -- -D warnings` 和 `cargo build --manifest-path tests/feasibility/grpc_container/Cargo.toml --locked --offline -q` 通过。

审查后候选准备前回收已死连接租约，hash cert/CRL 通过密封 memfd 使用 OpenSSL 对应 lookup 加载验证；坏证书、坏 CRL、空洞后坏项在 check 拒绝。不安全 TMPDIR 被拒绝。HTTP/1 后端收到请求头后立即断开，测试在后续 500ms 未观察到第二次连接。`cargo test --locked --offline --test reload -q -- --test-threads=2` 结果 33 通过、1 忽略、1 失败：本次 root 执行不满足权限用例非特权 UID 要求，未声称通过。

仍缺完整 TLS 错误原因与后端业务计数、真实连接 forced-collision、失败候选/CA 跨代、独立 DATA 帧和慢端背压等证据。用户已确认预算排除 OpenSSL 内部解析、证书对象及惰性缓存，不是 RSS 硬上限；Rill 持有材料和可计量副本仍计入。预算实现仍须独立审查，不将本轮作为完整验收。

错误候选与成功 reload 连续实测：`PASS invalid candidate rejected`、`PASS failed candidate retained old stream and backend`、`generation=2`、`PASS old bidi survives reload; new RPC backend=two`。容器 `151b48288c2be8aa79960b043fc733a332a3a2f386524b1e56abfc52d82fa132`、网络 `rill-grpc-acceptance-s20lxbia` 和临时目录已清理。

最终本轮重跑：`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、独立工具 fmt/clippy/build、`cargo build --release --locked --offline -q` 均通过；pool 单元 4 项、configuration 36 项、http_proxy 41 项、https 25 项通过。新增真实 h2c 后端收取请求头后断开且 500ms 无重连重放验证。最后一轮真实 Rill 四类 gRPC、同连接取消及错误/成功 reload 通过，容器 `6318cdd8b23a5831572561b505f399b4fb9b4ef2ee8fe2d38d2338d9b4a921c9`，网络 `rill-grpc-acceptance-3n0msinf`，均已按创建返回值删除，临时目录删除。

## 审查修正与补充验证

- hash 目录材料先以密封 memfd 的 OpenSSL 混合 PEM 路径验证所有证书/CRL，再以相应 hash lookup 类型验证；合法证书附合法 base64 但无效 DER CRL、坏证书、坏 CRL 和后缀空洞后的坏项均在 check 被拒绝。私有副本字节及清理名称在创建前计入候选预算。mkdtemp 后使用 parent FD、dev/ino/uid 身份回滚；`umask 0777` 回归验证没有遗留私有目录。上述实现尚待最终独立安全复审。
- `cargo test --locked --offline --test https tls_h2_bad_ca_and_name_fail_before_business_bytes -- --nocapture`：两种证书失败均为 502、日志 `InvalidCert`，后端没有收到业务 HEADERS/DATA。IP SAN 不匹配发生在 TLS 及 h2 控制帧之后，故测试允许协议前言/SETTINGS 而不是错误地把它们当业务字节。localhost 多地址首次试验被后续 ConnectRefused 覆盖诊断，最终以固定 127.0.0.1 测试。
- `cargo test --locked --offline --lib forced_group_collision_reuses_connection_but_callback_rejects_before_headers -- --nocapture`：cfg(test) 测试以真实 loopback Pingora Connector 和生产出生及 Handler 回调验证 A 成功、B 强制 group key 相同且复用 A，但 B 真实 token 不同，回调报告 `UPSTREAM_POLICY_MISMATCH`，后端未收到 B 业务头。故障注入不进入生产 CLI/二进制；这不是外部真实生产二进制可构造碰撞的证据。
- `cargo test --locked --offline --test https grpc_message_split_across_h2_data_frames_round_trips -- --nocapture`：真实 Rill TLS 入站、h2c 上游，gRPC 消息跨两次 DATA 发送；后端实际观察至少两个 DATA、重组并返回拆分响应。首版测试未并行驱动 h2 backend 导致超时，修复测试 driver 后通过。
- 最后一轮主 fmt/check/clippy/release、独立工具 fmt/clippy/build、HTTPS 28 项、HTTP proxy 41 项和碰撞测试通过。`python3 tests/feasibility/grpc_container/run.py --rill` 再通过 h2c/TLS h2 RPC 与 reload；容器 `fb0061887374b0d777712a2fa20912f3fa2175126dfe24157cffd732bba37aff`、网络 `rill-grpc-acceptance-43efm3_0` 及临时目录已删除。

仍缺更换 CA 的 TLS h2 旧长流跨 reload 全生命周期及慢读/慢上传背压恢复的真实证据；不能用现有 h2c reload 或大消息通过来代替。原先一次 WebSocket EOF 历史仍保留，未声称已修复。

## CA 跨代与双向背压实测

本轮扩展既有独立工具，两个 TLS 后端证书分别由临时 CA1、CA2 签发。运行 `python3 tests/feasibility/grpc_container/run.py --rill`：真实 Rill TLS h2 上游的旧 bidi 先连接后端 one；错误候选拒绝后旧流及新 unary 仍到 one；替换启动时指定 `SSL_CERT_FILE` 的文件内容为 CA2 并改配置目标后 reload，新 unary 成功到仅 CA2 信任的后端 two，旧 bidi 仍收发并正常结束。同轮 h2c 对照亦通过。

独立控制连接查询后端有界 producer 计数；业务连接单次发送 128 个 256 KiB 消息（累计 32 MiB，不聚合），使用容量 1 的 channel。暂停上传时本地 sender 停在 7/128；暂停客户端读取时后端 producer 停在 34/128，两个观察间隔均无前进。启动业务连接同 TCP 兄弟 unary 后，不等待其占用共享连接窗口的响应，先恢复慢流，再验证 128 个顺序消息和同 TCP 兄弟完成。h2c 与 TLS h2 各通过两次完整重跑。输出示例：

```text
STAGE upload pressure=7; sibling launched; resuming upload
PASS upload blocked at 7/128 then resumed; sibling same TCP
STAGE reader pressure=34; sibling launched; resuming reader
PASS reader blocked producer at 34/128 then resumed; sibling same TCP
PASS invalid candidate rejected
PASS failed candidate retained old stream and backend
id=400404-1793895601949893192-0 generation=2
PASS old bidi survives reload; new RPC backend=two
```

首次版本把同连接 unary 的响应作为恢复慢流的前提，重复运行观察到 60s `WriteTimedout`/H2 CANCEL，另一次 upload 阶段 `ReadTimedout`。共享连接窗口耗尽导致测试自等待是合理假设，但不声称已证明具体根因；失败记录保留。修改为独立控制连接且先恢复业务流后，连续两轮完整通过，不将此前失败覆盖或说成产品修复。最后一次容器 `3c2a532592aa2f419ee35691d3dee8b21c2b57c7cd950fa1336ed150efa49372`、网络 `rill-grpc-acceptance-19kkjgp3`、临时 CA/私钥及工作目录清理完毕。

父审查补充：空 hash 目录的材料和私有副本目录元数据也计入预算；私有副本拥有独立 permit，删除后释放，清理失败保守保留额度；子目录创建后打开失败时相对 root FD 回滚。该最终生产修正需父独立复审。

AC01 证据：上述真实四类 RPC、状态/trailers、512 KiB 和独立 DATA 测试；AC02：真实同 TCP 取消/deadline、失败及 CA1→CA2 成功 reload 的旧流/新请求，以及本节慢流验证；AC03：TLS 错误与不降级、真实 connector 强制碰撞、证书/CRL 和受限目录测试、无重放以及本节有界背压测试。计划勾选仍留给父最终裁定。

## FD 耗尽时的私有目录回收

父复审发现：子目录打开遭遇 `EMFILE` 后，私有根目录 Drop 再次 `openat` 核验身份也会失败，导致可清理目录和预算滞留。现使用已持有 parent FD 的 `fstatat(AT_SYMLINK_NOFOLLOW)` 校验目录类型、uid、dev/ino，随后 `unlinkat`；身份不符或清理失败仍保守保留预算。

`cargo test --locked --offline -q --lib private_directory_drop_cleans_and_releases_budget_when_fds_exhausted -- --nocapture`：通过。隔离子进程以 `ulimit -n 64` 耗尽 FD，再 Drop 真实私有目录，断言目录已删除且进程预算允许重新预留 9 × 7 MiB。`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`git diff --check` 均通过；最终父会话复测如下。

## 最终独立复审与收口

独立 reviewer 复核 trust、pool、TLS、配置准备、配额和 vendor 接口补丁，逐项关闭坏 mixed PEM、死亡连接回收顺序、不安全临时父目录、空目录元数据计费、副本 permit 生命周期、根/子目录失败回滚及 FD 耗尽清理等发现。最终结论 `OK with notes`，没有未解决的代码发现。reviewer 的结论来自源码审阅，以下运行结果由父会话实际执行。

最后一次生产清理修复之后，父会话重新执行并通过：

```sh
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo build --release --locked --offline -q
cargo test --locked --offline --doc -q
cargo fmt --manifest-path tests/feasibility/grpc_container/Cargo.toml --check
cargo clippy --manifest-path tests/feasibility/grpc_container/Cargo.toml --locked --offline --all-targets -q -- -D warnings
git diff --check
```

非 root 全套在独立 0755 工作区执行，含本轮 vendor 源码。入口路径记录于 `/var/tmp/rill005-suite-path`。以 root 使用 `CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target cargo test --locked --offline --no-run --message-format=json` 构建，按 Cargo JSON 选取八个 `profile.test=true` 工件，逐个运行 `runuser -u nobody -- <executable> --test-threads=2`。仓库共享 target 符号链接未改变。

| 顶层工件 | 通过 | 忽略 |
| --- | ---: | ---: |
| lib | 48 | 0 |
| main | 0 | 0 |
| configuration | 36 | 0 |
| http_proxy | 41 | 0 |
| https | 28 | 0 |
| reload | 34 | 1 |
| static_files | 27 | 0 |
| feasibility | 31 | 0 |

最终 **245 passed、0 failed、1 ignored**。子进程回归的单项汇总不重复计入顶层总数。忽略项仍要求 `127.0.0.1:53` 专用 DNS fixture，未启用。原始结果：[suite.log](data/019-http2-grpc/suite.log)。最后一轮未复现历史 WebSocket EOF，但没有据此宣称已确定或修复其根因。

父会话还在最终 release 上实际运行：

```sh
python3 tests/feasibility/grpc_container/run.py --rill \
  --image 394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853
```

结果六组场景全部通过：h2c/TLS h2 各四类 RPC、状态和响应 trailers、同 TCP 取消与 deadline、32 MiB 双向背压恢复，以及失败候选和成功 Reload。TLS h2 分支在更换 CA 文件后连接 CA2 签发的新后端，旧 bidi 流继续使用旧连接，新 unary 到 backend two。上游窗口共享，测试不要求耗尽连接窗口时其他流仍无限前进；恢复后兄弟流在同一上游 TCP 完成。

[最终矩阵原始输出](data/019-http2-grpc/grpc.log)记录 `passed=true`、`cleanup=true`、`cleanup_errors=[]`。容器 `3db098ce935ed00f6a207e7521e499f51ba68238a6d2862f13667ecdf097edcb`、网络 `rill-grpc-acceptance-9p_qm5ha`、对应私有目录及私钥均已核验清理。未下载镜像、依赖或其他工件，未操作生产 Caddy。

### 本地补丁来源与计量边界

父会话对本地原始 `pingora-core-0.9.0.crate` 计算 SHA-256，与本节开头及 `vendor/pingora-core-0.9.0/RILL-SOURCE.txt` 一致；逐文件比较除列明修改、删除及来源记录外均相同。生产补丁五个文件涉及连接出生钩子、per-peer 信任 store、严格 ALPN 和原锁定 zstd 版本。另将上游 HTTP/1 测试中的字符写为 Rust Unicode 转义，以遵守项目字符规则，运行时测试字节不变；该非生产表示修改后重新通过主 fmt/check/clippy/release。生产五文件的完整可审差异：[vendor.patch](data/019-http2-grpc/vendor.patch)；非生产测试文件另经逐字节核验，仅替换字符的源码表示为等价 Unicode 转义，未改测试字节。上游 LICENSE 原样保留，未携带其示例私钥或独立 Cargo.lock，没有修改全局 Cargo registry。

新增直接依赖仅复用已锁 `openssl-probe =0.1.6`，无既有版本升级；本地 patch 替代同版本 pingora-core，tonic/prost 仍仅在独立测试工作区。实际 rustc/cargo 为 1.99.1，未验证 MSRV 1.95；release 是 x86-64 动态链接 ELF，不声称完全静态。

用户批准的 CA 额度只计 Rill 原材料、可计量副本和元数据，排除 OpenSSL 内部解析、证书对象与惰性缓存，不是 RSS 上限。清理身份不符或真正失败时保守保留对应额度，不删除替代对象；瞬时 FD 耗尽已有回归证明可使用原 FD 清理并归还额度。候选冻结后外部 CA 更新须 Reload；每上游 h2 连接最多 16 流，出生记录全进程 4096，并遵守后端更低限制。完整材料相等性及唯一 token 才是身份依据，group key 不作为授权。通用请求 HTTP trailers 不在冻结 gRPC 范围，不因四类 RPC 通过而宣称支持。

005-T01–T03、AC01–AC03 和 G03 已验证完成；其余阶段和总目标未据此完成，不自动启动 006，也未恢复 OTLP、tracing 或 metrics。用户独立提交的 AGENTS.md 保持不变。
