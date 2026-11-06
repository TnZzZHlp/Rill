# 020：公网动态代理验证记录

本记录保留 006 各实施切片、失败与修复历史；早期“未完成”描述不代表最终状态。006-T01–T03、AC01–AC03 已经独立复审和真实隔离验收，最终收口见末节；完成提交以包含本记录的 Git 提交为准。

已按授权离线添加直接依赖 `url =2.5.4`（默认 feature，MIT OR Apache-2.0，URL 解析及规范化）和 `percent-encoding =2.3.2`（默认 feature，MIT OR Apache-2.0，URL 一次解码；原已锁定）。URL 默认 IDNA 路径引入下列新传递包，版本和许可证取自本地 registry 包元数据：

- MIT OR Apache-2.0：displaydoc 0.2.7、form_urlencoded 1.2.2、idna 1.1.0、idna_adapter 1.2.2、stable_deref_trait 1.2.1、utf8_iter 1.1.0、utf_types 1.0.0。
- MIT：synstructure 0.14.0。
- Unicode-3.0：icu_collections 2.4.0、icu_locale_core 2.4.0、icu_normalizer 2.4.0、icu_normalizer_data 2.4.0、icu_properties 2.4.0、icu_properties_data 2.4.0、icu_provider 2.4.0、litemap 0.8.3、potential_utf 0.1.6、tinystr 0.8.5、writeable 0.6.4、yoke 0.8.4、yoke-derive 0.8.4、zerofrom 0.1.8、zerofrom-derive 0.1.8、zerotrie 0.2.5、zerovec 0.11.9、zerovec-derive 0.11.7。

新增依赖用于 URL 表单解析、IDNA/Unicode 规范化及其派生宏；未升级原有锁定包。第一次 `cargo check --offline -q` 离线生成锁文件并通过。

- `cargo test --locked --offline -q --lib proxy::dynamic::target::tests`：1 项通过，仅验证未接入的目标解析边界。
- `cargo clippy --locked --offline --all-targets -q -- -D warnings`：通过。
- rustc 1.99.1 的 `Ipv4Addr::is_global` 仍不稳定，不能作为稳定公开 IP 判断实现。

尚未执行真实服务、隔离网络、完整质量门禁、reload 与容量测试。URL 提取、Location 和每目标连接计数语义仍待用户确认；不得以本记录代替最终验收。

## 配置和地址策略初步切片

统一 check 现在解析动态动作，检查默认鉴权及分支控制流，并为可能的 HTTPS 目标准备 CA。运行时尚不支持转发：动作检查当前请求的鉴权证明后以 `DYNAMIC_UNSUPPORTED`、503 失败关闭；不访问目标。地址候选纯函数及 128 活动许可接口尚未接入 DNS 或连接。IPv4 特殊段以本机 Rust 1.99.1 `core/src/net/ip_addr.rs` 为参考，并额外拒绝多播和过渡段；IPv6 保守只接受 2000::/3，排除特殊用途、文档及 6to4。调用方尚需提供完整本机接口集合，且必须在每轮 DNS 后调用候选检查。每目标物理连接计数、DNS、池、安全转发和隔离网络验收仍未实现。

本轮执行：`cargo test --locked --offline -q --test configuration -- --test-threads=2`（37 通过）；`cargo test --locked --offline -q --test configuration dynamic_proxy_requires_auth_on_every_reachable_path`（通过）；`cargo test --locked --offline -q --lib proxy::dynamic`（3 通过）；`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`（通过）。未联网，未执行真实目标连接。

## DNS 与连接配额候选（仍未开放转发）

用户已授权 `nix =0.31.3` 增加 `net` feature，离线对比 feature 修改前后 `Cargo.lock` 完全一致，没有新增传递包或升级。每次动态请求在鉴权后非排队获取 128 活动许可，并调用安全 `getifaddrs`；空接口、采集失败及接口数超 4096 均失败关闭。解析使用与固定上游共享的 16 个 blocking DNS 槽，任务结束前持有其槽，5 秒超时不发布迟到结果，不缓存目标；逐轮检查不超过 64 个候选及监听地址，任一候选拒绝则整轮拒绝。`server upstream-allow` 不参与。当前批准公网目标在检查完成后仍返回 503，不发起上游连接。动态入口 prefix 后取当前 raw path-and-query 一次解码仅为待确认候选语义，不构成用户最终冻结；Location 和每 origin 物理连接语义亦待用户确认。

对 005 所用本地 vendor `pingora-core 0.9.0`（来源哈希见 019）本轮附加补丁：`PeerOptions` 增加可选新建连接 reservation hook；`TransportConnector::new_stream` 在 TCP/TLS 前获取 reservation，offload 任务持有该值至真实任务退出，连接出生成功后以 `SocketDigest` 交接。默认 None，不修改固定上游的出生 token 检查。参数化 origin limiter 用弱 socket 生命周期回收活连接额度，尚未接入生产 peer，8 的生产语义未批准；新 hook 的所有连接路径、取消及复用仍需独立审查与进一步测试，不能视作阶段验收。

新增真实 rill 回归：临时 loopback 后端与动态动作，请求私网 literal 得 403，后端无连接；仅本机，无外网流量。运行 `cargo test --locked --offline -q --test http_proxy dynamic_proxy_denies_private_target_before_connecting` 通过；`cargo test --locked --offline -q --lib proxy::dynamic` 5 项通过（含真实 loopback connector 新连接配额与回收）。`cargo test --locked --offline -q --test configuration -- --test-threads=2` 37 项、`cargo test --locked --offline -q --test http_proxy -- --test-threads=2` 42 项、`cargo test --locked --offline -q --test https -- --test-threads=2` 28 项通过。`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo build --release --locked --offline -q` 通过。尚未创建或运行隔离 namespace/DNS fixture，等待用户单独批准。动态转发、header 隔离、Location、长流许可及池复用未实现；所有 006 验收保持未完成。

## 用户确认及运行接入（验收未完成）

2026-11-06 用户确认 origin 8 条物理连接（含握手、空闲和旧代）、当前 URI 的 prefix 后 path-and-query 一次解码、相对 Location 先解析后同源改写，以及只在网络和 mount namespace 内运行临时隔离验证。动态代理已接入 HTTP/1 上游、共享快照 CA、005 出生与池身份检查和跨代 origin 额度；动态与固定连接池身份隔离。每次请求重新解析，不复用固定上游 60 秒 DNS 缓存。旧段所述允许目标一律 503 为历史状态，不代表当前实现。

执行 `cargo build --locked --offline -q` 和 `unshare --net --mount python3 tests/dynamic_proxy/netns.py`：第三次及最终运行输出如下；前两次分别因测试断言大小写错误修正，非服务错误。fixture 仅在独立 namespace 启用 lo 的 8.8.8.8/32、无默认路由，临时 bind mount 私有 resolv.conf，真实 Rill、临时 Python HTTP/HTTPS 和本地 UDP DNS；临时证书、进程和目录在 finally 清理，namespace 随进程退出。

```text
PASS dynamic HTTP forwarding, header isolation, relative Location and wildcard self denial
PASS dynamic HTTPS DNS and hostname verification
PASS HTTPS hostname mismatch before business request
PASS mixed DNS and public-to-private rebinding denial
PASS eight physical connections and ninth denied
```

`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo build --release --locked --offline -q` 通过；`cargo test --locked --offline -q --lib proxy::dynamic` 5 项、configuration 37 项、http_proxy 42 项、https 28 项通过（集成命令带 `--test-threads=2`）。上述结果不涵盖全部验收：尚未证明连接空闲与跨 reload 额度、128 活动请求、坏 CA、65 候选、认证真实转发、长流取消、无重放及动态错误链完整行为；未勾选任务和验收。

## 扩展隔离矩阵重跑

`cargo build --locked --offline -q && unshare --net --mount python3 tests/dynamic_proxy/netns.py` 最终通过：

```text
PASS dynamic HTTP forwarding, header isolation, relative Location and wildcard self denial
PASS token authentication and credential isolation
PASS dynamic HTTPS DNS and hostname verification
PASS HTTPS bad CA and hostname mismatch before business request
PASS mixed DNS, 65-candidate capacity and public-to-private rebinding denial
PASS eight physical connections across reload, ninth denied, idle reuse
PASS 128 active requests and immediate capacity denial
PASS no replay after upstream receives business request
```

脚本另外核验原查询、外部 Location 原样返回；动态和固定池身份显式区分。8 条活动连接在成功 reload 后仍占同一 origin 额度，释放后请求成功；此项不单独证明空闲池满额或所有握手取消边界。128 活动请求测试在 16 个独立 DNS origin 上逐步进入，额外请求立即 503。临时服务仅限命名空间，DNS、证书、配置、子进程及 bind mount 均由 fixture 清理。未保存包含凭据的原始日志。

随后 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo build --release --locked --offline -q`、`git diff --check` 通过；动态模块 5 项、configuration 37 项、http_proxy 42 项、https 28 项通过；`cargo test --locked --offline -q --test reload chain_proxy_stream_keeps_old_snapshot_across_reload` 与 `cargo test --locked --offline -q --test reload authentication_token_reload_and_bad_candidate_preserve_old` 各 1 项通过。旧段列为未测试的坏 CA、65 候选、128 活动请求及 token 认证已由本轮覆盖。仍未单独验证 Basic 真实流量、动态失败候选与旧长流、握手取消、完整 WebSocket/SSE、空闲池满额和强制碰撞动态实例；不以固定代理测试替代这些证据。计划勾选和最终验收留待父会话复审。

同一 fixture 随后追加真实 socket 对端端口比较及失败候选：同 origin 两次请求复用同一后端连接，另一 origin 不复用；8 个旧请求存活时，错误配置 reload 被拒绝，恢复配置后成功 reload，期间第九条连接均受限，旧请求最终完成。最终再次执行 `unshare --net --mount python3 tests/dynamic_proxy/netns.py`：

```text
PASS dynamic HTTP forwarding, header isolation, relative Location and wildcard self denial
PASS token authentication and credential isolation
PASS dynamic HTTPS DNS and hostname verification
PASS HTTPS bad CA and hostname mismatch before business request
PASS mixed DNS, 65-candidate capacity and public-to-private rebinding denial
PASS same origin connection reuse and cross-origin isolation
PASS failed and successful reload preserve eight old requests, ninth denied, idle reuse
PASS 128 active requests and immediate capacity denial
PASS no replay after upstream receives business request
```

仍未单独验证 Basic 真实流量、握手取消、动态 WebSocket/SSE 和空闲池满额；跨 origin socket 差异不是强制 hash 碰撞验证。最终独立安全复审和非特权完整套件仍由父会话执行。

## 安全复审修正及剩余验证

复审发现并修复：fixture 现在自动创建网络及 mount namespace，继承父进程两种 namespace 的 FD，逐一比对设备和 inode；在任何 ip/mount 前确认均已变化、仅 lo、IPv4/IPv6 各路由表无路由。负面测试以相同 namespace FD 及仅 unshare 网络的子进程分别调用 internal 模式，均在修改前退出；直接 `python3 tests/dynamic_proxy/netns.py` 安全自建双 namespace。首次实现错误使用继承 sysfs 检查接口，虽在修改前拒绝但无法完成验证，已改为 `ip -o link`。不依赖与 `/proc/1` 比较。

同源 Location 的相对或绝对 fragment 移到外层 Location；目标 URL 入站仍拒绝编码的 fragment。局部 503 错误链中动态目标解析 400 和地址拒绝 403 均保留 503，普通路径仍返回 400/403：`cargo test --locked --offline -q --lib dynamic_error_chain_preserves_unavailable_for_invalid_and_denied_targets -- --nocapture` 四种真实 Session 路径通过。DNS 返回后重新采集实际接口，每次连接选择（包括复用）前再检查固定候选，不二次 DNS；采集失败拒绝。移除先前接口数 4096 人为限额。检查与网络配置变更不能原子化，检查后管理员改变网络属于可信控制面边界。

`cargo test --locked --offline -q --lib forced_group_collision_reuses_connection_but_callback_rejects_before_headers -- --nocapture`：基于既有真实 connector 碰撞测试，B 使用 dynamic 标记、HTTP/1 协议和目标主机身份，强制同 group 复用 A 后生产回调拒绝，后端未见 B 业务；1 项通过，无生产故障注入入口。

最终 fixture 新增 Basic HTTP/HTTPS（空、重复和错误凭据 401/challenge 且后端计数不增；成功隔离认证、token 头及 Cookie，普通过滤器再添加的 Cookie 与 Set-Cookie 仍清除）、TLS 八个真实握手 reservation 的取消/关闭回收、八条不同后端 TCP 的空闲池复用及 CA 更换跨代容量、旧 SSE/WS 在新策略 reload 后继续且二者占用 128 活动槽。128 测试逐个等待后端实际收到后才发下一请求；SSE 和 WS 收取按事件及帧累计，不假设单次 recv。握手取消测试依照实际 socket 是否仍存活检查：若仍存活第九条必须拒绝，关闭后新连接允许。

fixture 开发中曾出现：Basic 测试误将未属该站点的普通 X-Token 当认证头，已添加实际声明该认证头的另一规则；SSE/WS 起初假设一次 recv 即完整或在容量测量前关闭 WS，已改成完整累积并保持连接；idle 测试后未重置 hold 事件导致后续断言失败，已重置。关闭受测 TLS 空闲连接时后端曾打印预期 BrokenPipe 堆栈；并未全局吞掉异常。上述失败不是测试通过证据。

最终命令 `cargo build --locked --offline -q && python3 tests/dynamic_proxy/netns.py` 通过（脚本自身二次隔离）；下列为原始 PASS 行，另有主动关闭受测 idle TLS 连接触发的 Python backend BrokenPipe 堆栈，保留为预期关闭现象，未吞掉异常：

```text
PASS post-DNS local-interface recheck and Location fragment roundtrip
PASS dynamic HTTP forwarding, header isolation, relative Location and wildcard self denial
PASS token authentication and credential isolation
PASS Basic HTTP/HTTPS challenge, duplicate rejection and final header isolation
PASS dynamic HTTPS DNS and hostname verification
PASS HTTPS bad CA and hostname mismatch before business request
PASS handshake pre-reservations, cancellation and release after real socket closure
PASS mixed DNS, 65-candidate capacity and public-to-private rebinding denial
PASS same origin connection reuse and cross-origin isolation
PASS eight observed idle sockets, reuse, changed CA across reload, death recovery
PASS failed and successful reload preserve eight old requests, ninth denied, idle reuse
PASS SSE and WebSocket across reload and held active capacity
PASS 128 active requests and immediate capacity denial
PASS no replay after upstream receives business request
```

同轮 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo build --release --locked --offline -q` 和 `git diff --check` 通过；动态模块 5 项、上述两个 handler 单测各 1 项、configuration 37 项、http_proxy 42 项、https 28 项及两个 reload 定向各 1 项通过。SSE/WS 验证期间 reload 改变新配置 prefix，新请求按新配置拒绝，旧流继续；随后恢复配置并与 126 条后端已确认活动的请求共同占满 128 槽。证据不代替父会话独立源审和非特权完整套件。

最后补充：`reject` 在响应期限本身超时的分支也保留既有错误链 503。此修改后重新执行上述自动隔离完整矩阵，14 个 PASS 行全部通过，主动关闭 idle TLS 所致 BrokenPipe 仍照实输出；fmt/check/clippy/release、两个 handler 单测、configuration 37、http_proxy 42、https 28、两个 reload 定向和 diff 检查均重跑通过。最终独立复审和非特权完整套件仍待父会话。

握手测试最后改为逐条建立客户端并等待对应后端 TCP 接受，以确保取消的第一个客户端和观察的第一个真实 socket 一一对应；`python3 tests/dynamic_proxy/netns.py` 完整重跑 14 项 PASS。503 单测增加响应期限已过分支，仍保留 503；首次修改单测时遗漏使用新变量，Clippy 报 unused-variable，补齐期限设置后单测、Clippy 和格式检查通过。

## 最终独立复审与父会话复测

独立 reviewer 已复核全部动态模块、配置与鉴权路径、最终头隔离、handler、pool 及 vendor 新建连接预留钩子。四项生产/隔离边界发现均关闭：修改前验证双 namespace、fragment 留在外层 Location、原始 503 保持、DNS 后及连接/复用前重新采集本机地址。最终结论 `OK with notes`，未发现未解决的生产阻塞项。reviewer 未自行运行测试，下列结果由父会话实际执行。

reviewer 另指出 fragment 回归曾假定单次 TCP recv 取得完整状态行。父会话将其改为三秒期限内有界累计至完整响应头（最多 64 KiB），保持原状态和 round-trip 断言，再完整复跑隔离矩阵通过；没有修改生产行为，也没有增加工具或依赖。

父会话执行并通过：

```sh
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo build --release --locked --offline -q
cargo build --locked --offline -q
cargo test --locked --offline --doc -q
git diff --check
python3 tests/dynamic_proxy/netns.py
```

隔离 fixture 的默认入口自行创建并核验独立 network/mount namespace，初始只有 lo 且路由表为空，之后才配置模拟公网 IP 和私有 resolver 挂载；无外部路由，不访问真实公网或元数据服务。父会话还传入继承的 namespace FD，分别在原 namespace 和仅隔离 network 的子进程调用内部入口，两者均在任何修改之前拒绝。完整运行前后宿主 `/etc/resolv.conf` 的 SHA-256 相同。临时目录、证书、私钥和 Rill 进程已清理，namespace 随子进程退出；没有遗留 `rill-dynamic-netns-*` 目录。

最终矩阵 **14 项 PASS**，原始输出见 [netns.log](data/020-dynamic-proxy/netns.log)。主动关闭空闲 TLS 连接产生的 Python BrokenPipe 堆栈保留在输出中；该次进程退出码为 0，所有明确断言通过，不将其隐藏或冒称生产异常已修复。

非 root 全套在独立 0755 工作区 `/var/tmp/rill006-suite-TIV3qh` 中执行。复制当前源码、配置、vendor 及测试输入后，以 root 使用 `CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target cargo test --locked --offline --no-run --message-format=json` 构建；从 Cargo JSON 选择八个 `profile.test=true` 工件，逐个执行 `runuser -u nobody -- <executable> --test-threads=2`。未改动仓库共享 target 符号链接。

| 顶层工件 | 通过 | 忽略 |
| --- | ---: | ---: |
| lib | 54 | 0 |
| main | 0 | 0 |
| configuration | 37 | 0 |
| http_proxy | 42 | 0 |
| https | 28 | 0 |
| reload | 34 | 1 |
| static_files | 27 | 0 |
| feasibility | 31 | 0 |

合计 **253 passed、0 failed、1 ignored**，不重复累计子进程测试摘要。忽略项仍是要求专用 `127.0.0.1:53` resolver 的历史 DNS 用例，本次动态隔离 DNS 不改变宿主，也不擅自启用该用例。[suite.log](data/020-dynamic-proxy/suite.log)保留原始全套输出；最后的非生产 fixture 累计读取修正不影响此 Rust 套件结果，其后隔离矩阵已重跑。

### 交付边界

用户已明确确认每 origin 八条存活连接含握手、空闲及旧代，以及路径查询整体一次解码、相对 Location 同源改写和临时验收夹具。动态 DNS 每请求重新查询，不使用固定代理的 60 秒缓存；所有候选检查后连接固定地址，池复用前也复验自身入口。复核后可信管理员再次改变接口/路由与连接不能原子化，此可信控制面边界没有被误称为消除。匿名模式明确接受开放代理滥用风险，public-only 不是用户授权控制。

本轮 vendor 只扩展已锁定的 pingora-core 新建连接 reservation 接口：TCP/TLS 前准入，实际 offload 任务持有，出生后移交 socket 生命周期；固定上游默认不启用该可选预留钩子，005 的精确身份守卫保持。相对 005 的可审差异：[vendor.patch](data/020-dynamic-proxy/vendor.patch)。原上游来源、许可证及 005 补丁见 [019](019-caddy-http2-grpc.md)，未修改全局依赖缓存。新增 URL/IDNA 依赖均离线，未升级既有锁定版本；nix 仅启用已锁 `net` 功能。实际 rustc/cargo 为 1.99.1，未验证 MSRV 1.95。

006-T01–T03、AC01–AC03 已通过实际证据关闭；G04 仍需 007，不勾选其整体完成。本阶段未新增 CONNECT/SOCKS、正文变换、观测导出或生产切换，未操作现有 Caddy，不自动启动 007。用户的 AGENTS.md 保持不变。
