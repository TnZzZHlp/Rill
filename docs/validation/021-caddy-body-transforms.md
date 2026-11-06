# 021：正文变换实施记录

## 状态与边界

007 已完成配置、候选编译、实际路径登记及普通/动态代理、静态、respond、handle-errors 的共享响应变换。独立复审发现均已修复，最终源码结论 OK；父会话最终非特权全套及隔离回归通过，T01–T03、AC01–AC03 与 G04 关闭，完成提交以包含本记录的 Git 提交为准。不涉及 008、生产 Caddy 或观测导出能力。下文保留各切片的 BLOCK、失败与修复历史，最终状态以末节收口为准。实际工具链 `rustc 1.99.1 (58cf8f9c3 2026-10-13)`，没有运行 MSRV 1.95。

旧 `error-page` 只能配置在旧静态语法，不能与 chain 正文声明组合；没有扩展 `serve-files` DSL。真实错误正文变换由已有 `handle-errors` 中的 respond/serve-files 路径提供。共享静态错误页代码保留旧行为，不能把库级可构造组合宣传为新 DSL。

## 模块与算法

- `config/parser/chain_body.rs`：显式 MIME 属性及额外 mime 子节点，严格类型、数量、重复、未知字段、level=3；保留原始声明和每条规则 Origin。Profile/Snippet 不扩展。
- `body_transform/{model,compile,replacement}`：顺序规则、严格 `$1`–`$9`/`$$`；沿规则、分支、continue、最多四次 reenter、错误链计算可能累计最大规则数（16），encode 主/错误链合计最多一次；运行时同样限制。程序编译复用候选 8 MiB/进程 64 MiB、2 MiB NFA、4 KiB 模式及重复 1000 限制。
- `http_policy/state.rs`：与原有头过滤器共用 256 实例/64 KiB 元数据/36 KiB 冻结捕获机制，只登记实际执行的声明；错误链保留已到达声明。respond 文本由候选 Arc<str> 持有，不每请求复制完整正文。
- `literal.rs`：真实模式前缀 KMP，非匹配连续段批量输出；空 literal 按完整逻辑字节边界插入，错误后 poison。模式、replacement 引用候选，前缀表由请求预算预留。
- `regex_stream.rs`：明确 PikeVM Cache，不使用高级 Regex 自动池；缓存/捕获工作区复用既有显式 64 MiB workspace admission。HIR maximum_len 有限时保留实际最大长度、额外右侧四字节及左侧四字节上下文；搜索 span 在原 pending 内保持非零起点，防止裁剪制造新的 `\\A`。推迟可能依赖未到达上下文的匹配；匹配顺序、捕获及相邻空匹配抑制按 bytes batch 对照。maximum_len=None 只做受共享容量约束的 EOF fallback，不用任意窗口，不落盘；EOF 上按同一完整逻辑序列展开。有限模式有提前输出测试。
- `decode/encode/replace/pipeline/limits/budget`：真实 blocking worker，输入/输出 channel 各一槽，逐个输出写完再拉下一个；每块至多 16 KiB（契约上限 64 KiB），不聚合 64 MiB。decode → 实际规则序列 → encode；gzip MultiGzDecoder 严查 CRC/成员/截断，zstd decoder windowLogMax=20，encoder windowLog=20/checksum；gzip level3/window32KiB，zstd level3/window1MiB，无字典或多线程调用。解码累计 64 MiB、100×已消费编码输入+64 KiB，所有规则生成输出共享累计 64 MiB，编码交接也限 64 MiB。
- `headers/response`：原始状态/MIME/encoding/no-transform/Range/协议判断在通用头动作之前；最终表示/framing 修正在之后。实际变换删除全部源 ETag/Last-Modified/Content-MD5/原长度，合并 Vary；HEAD 不跑正文，长度不可确定时删除；204/304 无正文。代理只在一次上游副本移除待变换 GET/HEAD 的 If-None-Match/If-Modified-Since，不移业务条件、Range/If-Range，不重放。静态先按真实源 MIME 决定 identity/sidecar 和是否禁用源条件缓存，未变换 sidecar 保留 304/Range。

## 内存、取消与背压证明范围

预算是 Rill 自有及额外持留的正文变换存储，不是进程 RSS 或整条 HTTP IO 的硬上限。每请求实际 allocation capacity 合计 1 MiB，全进程 64 MiB，64 响应额度。codec 内部上下文/工作区不计字节预算，但其真实生命周期持有响应许可。

1. `Buffer::new` 在 allocation 前同时预留请求/进程容量，检查真实 Vec capacity。复审后已删除旧 `Buffer::grow`：所有无界 stage 改共用一次连续 arena，未用 region 也全额收费；按相对 offset 紧凑复用，不逐块全量新建并复制 pending。pending 只能使用扣除存活工作区/信用后的剩余，不保证独享 1 MiB。该共享口径由用户在独立复审后最新确认，不将旧约半 MiB 过早失败追认为已获批准。
2. proxy 源是框架当前输入 Bytes：同步 push 中先预留精确自有 Buffer、一次有界复制、立即 drop 原 Bytes，再提交 worker；不 clone、跨等待持留未知源 capacity、不拆片隐藏共享大底层。源过大直接失败。原框架接收/解析/flow-control 缓冲仍属原框架边界。
3. respond 从策略 Arc<str> 直接一次复制到预留 input；变换静态文件先在同一 ResponseBudget 分配 Buffer，真实 blocking read 任务持有 Buffer，读完直接提交，取消不提前释放真实读任务存储。listing/cache 是 005 原有有界 producer：确定变换后、启动 pipeline/新增 await 前按 listing Vec.capacity / cache 原 Box 全长度额外持券；源 owner 到 input copy/drop 的重叠同时计费。共享 cache 的旧全局拥有引用仍由 005 预算管理。
4. 输出通过 `Bytes::from_owner` 绑定 BufferPermit 和 ResponseBudget（后者持请求快照）。clone、slice、框架排队继续持同一真实 allocation 许可；不按长度归零、write 返回或消费完毕提前释放。下游默认 compression level=0 不深拷贝。Pingora core h1 payload chain、h2 reserve_and_send 使用原 Bytes owner。
5. 既有 h2 0.4.20 `src/codec/framed_write.rs:240–258` 小 DATA/大 DATA 前缀会复制到原连接 encoder.buf（DEFAULT_BUFFER_CAPACITY=16 KiB，chain threshold=256/1024）。这是既有 framing/IO 内部存储，与 HPACK/TLS/内核存储一样不冒称在 007 字节预算内；未提高框架窗口或增加 Rill 输出队列。框架 `Next::Data`/`last_data_frame` 若仍持原 owner，其原 allocation 仍计费。
6. CPU 安全点累计活动执行时间 2 秒，channel 输入/输出阻塞暂停计时；异步每次等待执行结果至多 2 秒。单个 PikeVM/codec 调用不可抢占。超时关闭输入/输出、清空待发布结果并禁止再 next；真实任务继续持许可直至退出。测试中故意昂贵 PikeVM 等待 2 秒失败，任务约 8 秒完成期间仍占第 64 个响应许可，且不发布迟到结果。另有输出阻塞 3 秒后可继续成功的测试，避免把网络等待误算 CPU。

没有使用平均 RSS 证明这些上限。

## 同版 Pingora 补丁

`vendor/pingora-proxy-0.9.0` 来源本地已锁 registry crate，SHA-256 `2467265ad39b56230b56149f1c03ca510f46c4bf4a25d6d4b7345e0f06cc46e7`；保留 Cargo.toml/Cargo.toml.orig/LICENSE 和原 src。新增默认关闭的 `response_body_streaming/push/next`，输出 `Chunk/InputDone/Finished`；只有 Finished 允许成功 EOS。新 `proxy_body_stream.rs` 逐 chunk await 下游后继续拉取，push 不等待 worker 消费，避免双向 channel 死锁。

h1/h2 upstream、cache-write、cache-hit、header EOS、Done、trailers 经过 drain；UpgradedBody 不改。非 opt-in 同步 body filter 和 Duration 延迟路径保持原样；transform 时关闭可排队的 proxy task API，不构造 expanded task Vec。trailers 在 EOF 变换 drain 完后发送，源 validator trailers 删除，其余保留。生产 HttpTask/core 未改。

## 依赖复核

Cargo.lock 只增加 rill 对已锁 flate2=1.1.10/zstd=0.13.4 的引用及 pingora-proxy 同版 path 替换；无包升级、下载、新测试框架。

- flate2 1.1.10：MIT OR Apache-2.0，直接 `default-features=false, features=["zlib-ng"]`；实际统一树为 any_impl/any_zlib/any_c_zlib/libz-ng-sys/zlib-ng，无新增 Rust 默认后端。
- zstd 0.13.4：BSD-3-Clause，直接 default-features=false；已有 pingora-header-serde 统一 default/arrays/legacy/zdict_builder feature，但新 pipeline 不加载字典、不启 zstdmt。现有 0.14.3 只留给原 Pingora core，不混用 API。
- 复用已锁 bytes/regex-automata/regex-syntax/Tokio；futures 只由原 Pingora 内部使用，未新增无用直接依赖。

## 实际检查与失败更正（持续更新）

下列都使用 `--locked --offline`，未启 ignored DNS fixture：

- 前任 partial 的 5 个小测试/fmt/check/clippy/release 记录仅代表其当时独立 prototype，不代表本次集成完成。
- 本次首次 `cargo check --locked --offline -q` 在手动缩小直接依赖 feature 前拒绝锁更新；改为 flate2 明确 zlib-ng、zstd 禁直接默认 feature，并移除未获准的默认后端锁条目后通过，旧包版本未变。
- 首轮新 integration 失败于 KDL 分号、测试只读到 chunk framing 而非 payload、gzip 构造在 Ready 前读输入导致握手死锁，以及测试默认 zstd 2 MiB window 被 20 上限拒绝；分别修正语法/完整读取、把需要输入的 decoder 创建移到 Ready 后、测试显式窗口。没有取消 CRC/截断/窗口拒绝断言。
- 上次 worker 因工具 30 分钟超时中断，不是产品构建失败。恢复后 `cargo test --locked --offline -q --test body_transform http2 -- --nocapture` 首先修复 fixture：PeerOptions 默认 max_h2_streams=1，首连接满槽不进 in_use_pool，原 reused 断言不能证明关 TCP。设置测试 peer=10，额外用 bad.fd()==good.fd() 的同 ConnectionRef 物理 ID 证明失败流及兄弟确为同连接；无需改生产关闭策略，测试通过。
- `cargo test --locked --offline -q --lib body_transform:: -- --test-threads=2`：最新 9 项通过（含隔离子进程 64 MiB/64 响应边界测试）。
- `cargo test --locked --offline -q --test body_transform -- --test-threads=2`：最终 17 项通过（含隔离 CPU 子进程）；覆盖 proxy/static/respond/error/branch/reenter、identity/gzip/zstd、原 metadata 旁路/SSE/gRPC/WS/no-transform/未知编码、validators/Range/HEAD/204/304、cache/sidecar/listing、提前输出/阻塞/取消/超限 abort/同连接 H2 trailers 及 Reload。
- `cargo test --locked --offline -q --test configuration --test http_proxy --test https --test reload -- --test-threads=2`：configuration 37、http_proxy 42、https 28 通过；reload 33 通过、1 ignored DNS、1 失败（root 下 watch_permission_denied 明确要求非特权 UID）。
- `cargo test --locked --offline -q --test static_files -- --test-threads=2`：24 通过，3 失败于 root-only `/root` 工作区降权后 PermissionDenied（check_rejects_unreadable_root、cache_does_not_bypass_new_permission_denial、spa_does_not_fallback_for_denied_file）；需父按既定 `/var/tmp` 可读副本 nobody 重跑，不能归为通过。
- `cargo test --locked --offline -q --lib -- --test-threads=2`：当时 62 项通过；随后增加 bounded regex 提前输出测试，最终与 body_transform 集成同次运行的全 lib 为 63 项通过。
- `cargo clippy --locked --offline --all-targets -q -- -D warnings`：当前通过，无新增全局 allow；首次指出单位错误类型/大 enum/参数过多/test module 顺序后按模块收口修复。
- `cargo build --release --locked --offline -q`：最终重跑通过。
- `cargo tree --locked --offline -e features -i flate2`、`cargo tree --locked --offline -e features -i zstd@0.13.4`：通过，统一 feature 如上。

最终连续执行 `cargo fmt --all --check && cargo check --locked --offline -q && cargo clippy --locked --offline --all-targets -q -- -D warnings && cargo build --release --locked --offline -q && cargo test --locked --offline -q --lib --test body_transform -- --test-threads=2`：全部通过（63 lib、17 integration）。`git diff --check` 通过，`git diff --cached --name-only` 空，target 仍为原外部 symlink。另对修改的 vendor 源运行 `rustfmt --edition 2021 --config skip_children=true`；没有更改未涉补丁的原源文件。

本次新增真实慢读测试在设置接收窗口 64 KiB 后停读 3 秒，随后完整接收并核对 8 MiB；共享输出测试以 16 条顺序规则累计输出在最终正文尚不超过 4 MiB 时触及共同 64 MiB 上限；无界待决增长拒绝在请求公共容量内，不静默返回 EOF。头前 prefix 表资源失败只返回一次 503，不进入 handle-errors。gzip 多成员跨成员匹配、CRC/截断/trailing、zstd 明确 21 窗口拒绝及 20 窗口膨胀拒绝均已实际运行。

最后源码自查修复了编译可能路径对 strip-prefix 本地缺失、无匹配 branch 和链尾错误链累计的漏算，并覆盖回归；另修复静态替换已确定但 identity 不可接受时源 If-None-Match 不得抢先返回 304，先禁止源条件再生成 406；专项 `cargo test --locked --offline -q --test body_transform encode_only -- --nocapture` 与最终连续门禁再次通过。未扩大 DSL。首轮追加共享限额测试曾以不匹配的 test filter 运行 0 项，已纠正为 `cargo test --locked --offline -q --test body_transform sequential_rules -- --nocapture`，1 项通过并包含在最终 17 项中。一次末轮 fmt-check 指出新 helper 未格式化，已运行 fmt 并以最终连续门禁通过为准。

待父独立复审及最终全套 nobody/隔离矩阵；不将 root 权限失败、未跑隔离项、未跑 vendor 专属测试写作通过。

## 父首次独立复审后的 checkpoint（历史 BLOCK，后续修复见末节）

父复审 50422292 指出三项 P1；此前门禁通过不构成这些问题不存在的证据。父另已报告可读 `/var/tmp` 副本 nobody 全套 279 pass/0 fail/1 ignored、netns14 pass、真实 gRPC 六场景通过及清理完成；父原始日志为 `/var/tmp/rill007-parent-{gates,netns,grpc}.log`，本 worker 不冒称独立重跑这些结果。现有 vendor 对 archive 的 215 行补丁由父保存在 `docs/validation/data/021-body-transforms/vendor.patch`；本轮未改 vendor。

1. **最终头限额**：新真实 HTTP 回归 `finalized_transform_headers_cannot_exceed_the_shared_header_limit` 首先复现失败：respond 的 text/html、text=old、x-pad 65483 字节使头动作结束时恰 65536，随后变换删除 CL、添加 Vary/TE 后却发送 200。失败日志 `/var/tmp/rill007-review-header-before.log`。修复共同 `Prepared::finish_budgeted` 为单一出口：406、HEAD、仅 Vary、active/default 分支全部先完成表示头，再统一 `http_policy::headers::check`，通过后才启动 worker，提交前超限 503；不进入错误链，不发第二响应。proxy/local/static 的应用头修改都在该调用之前。回归扩展为同一真实服务的 active GET、HEAD、406、仅 Vary 四条出口，修后通过。
2. **Identity 大小写**：新真实上游回归 `proxy_replaces_case_insensitive_identity_content_encoding` 首先失败，合法 `Content-Encoding: Identity` 的 old 未替换。失败日志 `/var/tmp/rill007-review-identity-before.log`。改 identity 为与 gzip/zstd 一致的 ASCII 大小写不敏感比较；未知、多字段、stacked encoding 仍旁路。修后返回 new 且移除原 encoding，通过。
3. **无界待决分配浪费/分块准入差异**：尚未修复，不把约半 MiB 实际门槛当作用户批准契约。现 `Buffer::grow` 每 16 KiB 新建完整 allocation 并持旧源复制，虽然两者都收费，但导致本可在共享总额内表示的输入过早失败，并可随实际输入块持留大小改变准入。父正在向用户澄清硬共享总 1 MiB 与 pending 上限口径；本 worker 先提交工程方案，未私改限额、豁免双 allocation 或下调阈值。候选方向为无界 pending 一次连续分配/多 stage 共享 arena，避免逐块全量双 allocation；必须计算源输入、decoded/stage/output 工作区及 EOF 上下级 pending 重叠，不能宣称 exact 1 MiB 正文必过。特别纠正当前输出峰值：consumer await write 持第一块、channel 持第二块、worker 在 blocking_send 前已分配第三块，至少为 3×CHUNK；H2 历史 owner 可再持留，绝不能按 write 返回提前归还。用户决议及完整实现前保持 BLOCK。

### 第 3 项候选 B 的安全设计（获最新用户回复之前的历史记录）

父有条件认可：仅在用户确认保留共享总额口径后实施 B，不采用各无界 stage 抢占全部剩余额度的 A。B 为一次连续分配的共享 pending arena，真实 allocation 全额收费，未用 region 也不退券；所有无界 stage 仅持 region ID、逻辑长度和相对 offset。移动/紧凑使用 safe `copy_within`，不使用 unsafe、allocator 豁免、落盘或任意 regex window。

- `RegexStream` 改为 pull 片段状态机：PikeVM 对当前 region 的完整逻辑 slice 执行，返回待发 source/capture 的相对 span 或候选 replacement 片段；driver 在短借用内复制到已有带券 stage Buffer，结束借用后才推动下一级/移动 arena。cursor、last_end、capture span 均相对 region，不随 arena 紧凑改变逻辑位置，不保存跨移动 slice/RefCell guard。
- 上级 EOF 的所有 match、empty suppression、capture/replacement 片段完成后才释放其 region；最后 stage Buffer 中的字节已独立拥有，随后才向下级传播该 stage EOF。上级尚需的原捕获字节始终保留；下级累积时源/目标不同 region 的真实重叠都占共享 arena，不能给每个 stage 独立最大 pending。
- 工作信用必须是同一请求/进程 reservation 的 RAII 分拆，未消费 credit 仍占准入，分配实物不再双计。普通源交接至少按既有 64 KiB 最大常规块、decoded 16 KiB、每 stage 16 KiB、输出至少 3×16 KiB 核算；bounded regex pending、KMP 表、静态 producer 额外持留和更大源输入也在同一总额内扣除。不得给出未核完的固定 896/880 KiB 承诺，更不得称 exact 1 MiB 正文必成功。
- 输出三块只是当前 channel/write 的最低工作峰值，不是框架 owner 持留上限；历史 Bytes 仍占券。信用不足必须按真实额度失败，或等待另行明确批准的有界回收，不能把写返回视作释放。输入 EOS 后才可实际 drop 已不需要的 decoder/source/decoded 与未用输入信用，再做 EOF 级联；释放是 RAII 对真实 allocation/credit 的释放，不是缩短 arena region 后伪造退券。
- 验证计划：review 的单 a*、496 KiB 正文以 16/64 KiB 提交都应成功且等于 batch；多无界 stage 的 capture/expansion/empty 级联与共享实际占用；接近完整可用工作容量边界及一字节不足；取消/晚结果/旧 owner 后真实额度恢复。以上不是当前实现已具备或测试已通过的声明。

本轮 1/2 修复后执行 `cargo fmt --all --check && cargo check --locked --offline -q && cargo clippy --locked --offline --all-targets -q -- -D warnings && cargo test --locked --offline -q --lib --test body_transform --test configuration --test http_proxy --test https -- --test-threads=2`：通过（63 lib、19 body_transform、37 configuration、42 http_proxy、28 https）；日志 `/var/tmp/rill007-review-fixes-tests.log`。这些当时结果不关闭第 3 项，也不替代后续父全套重跑。

## 用户最新确认后的 B 实施

用户最新明确回复“保留共享总额建议”，批准请求全部 Rill 正文 allocation capacity 合计 1 MiB、进程 64 MiB 仍为硬限，pending 可用量扣除同时存活工作区，不保证 pending 或每条规则独享 1 MiB；要求工程消除旧逐块整段双 allocation。现已实际实现 B，尚待父独立复审及修后全套/隔离矩阵，不勾 T/AC。前述 279/0/1、netns、gRPC 为修前父证据，不能当本次通过。

### 实际存储与工作信用

- 新 `arena.rs::PendingArena`：在 worker 准备阶段、输入开始前，先扣全部已有 bounded pending、KMP 表、每 stage 输出块、decoded 块、静态源额外持留和工作信用，再一次 `Buffer::new(budget.remaining())` 分配连续 arena。真实 capacity 全额收费并检查，unused region 不退券；最多 16 个 region 的 start/len metadata 不分配正文副本。append 只写已分配尾部，空间不足先 safe copy_within 紧凑现有 live region；实际仍不足立即失败。单一源 region 正常追加不反复 compact，不存在每 16 KiB 整段新 allocation/copy。
- 新 `budget.rs::Credit`：仅有无界 stage 时，预留 64 KiB 输入信用、3×16 KiB 最低输出信用。信用从原 ResponseBudget 同一请求/进程额度预留；`take` 将预留拆分成 BufferPermit，实物 allocation 不再重复收费。permit 的弱回收地址只在信用 pool 仍活着时归还 free；pool drop 只释放未消费额度，仍由 Bytes/worker 持有的 permit 独立计费到真正 drop。失败、取消、旧快照都沿同一 RAII，未新增资源等待。
- 普通输入从 64 KiB 信用中分配精确实际 capacity，未消费信用仍占准入，因此 16/64 KiB 输入分块不会改变初始 pending 可用量；超过信用的单源块明确失败，先 drop 框架 Bytes，不跨等待保留未知底层。静态 listing/cache 原源额外持留仍按 Vec.capacity/完整 Box 先计费，再算 arena；这不是豁免，也不保证任意大单块成功。
- 最低输出信用 48 KiB 对应 consumer await write、channel 单槽、worker 已分配后 blocking_send 的至少三个块。更多框架历史 owner 不是“免费第四块”：信用不足时只尝试同一预算的真实剩余额度，仍不足就失败，不新增等待、不提前释放 clone。输出 permit 使用后只有真实 Buffer data drop 才可回收信用。
- `pipeline.rs` 在真实输入 EOS 后先 drop decoder/source 和 decoded Buffer；提交最终输入时已 drop 未用输入信用，最后真实源块仍随 worker 持券。之后才做 stage EOF 级联，实际释放的额度可供额外输出持留使用。arena 自身不缩容、不按 region 清空伪造 deallocation，直到真实 worker 结束才释放。

### 匹配与借用安全

`regex_stream.rs` 现在统一为 pull fragment 状态机。bounded 模式仍用一次预分配的真实 maximum_len+上下文 Buffer，保留提前输出及左侧非零搜索起点；无界模式仅保存 arena region ID，EOF 前不匹配/不输出。PikeVM 在该 region 的完整逻辑 slice 上搜索；cursor、last_end、captures、待发 source span 都是相对逻辑 offset，不依赖物理 start。`replace.rs` 每次在短借用内把 fragment 的一部分复制到带券 stage Buffer，借用结束后才递归推进下级，因此后续 arena compact 不使 slice/offset 失效，也没有 RefCell 跨推进借用。上级全部 unmatched/capture/replacement 片段独立复制完成后才释放 region/engine，再向下级传播 EOF。compact/copy 都在 Clock 活动执行计时内检查，不当作 IO 暂停；晚取消仍由真实 worker/arena/输出 owner 持许可。

### 明确计算与实测边界

对测试的单 `regex "a*" "x"`、identity 输入输出、无额外源持留/无其他规则的具体 pipeline：1024 KiB − 输入信用64 − decoded16 − stage16 − 输出信用48 = **880 KiB arena**。这只是该配置的可验证计算，不是所有配置的固定承诺，更不是 exact 1 MiB 正文保证。`unbounded_arena_removes_double_allocation_and_has_one_shared_working_boundary` 实测同一 496 KiB、880 KiB−1、880 KiB 正文按 16/64 KiB 提交都成功并等于 Rust bytes batch；880 KiB+1 两种分块均明确失败。另运行无界规则下 gzip→zstd / zstd→gzip、小块输入的严格 codec 流。

三个无界 stage 的具体 arena 为 848 KiB（多两个 stage 块共32 KiB）：400 KiB 输入经捕获追加、贪婪全体/空匹配、再捕获追加的级联，对 16/64 KiB 分块及空/小正文均等于 batch；上级源与下级 pending 重叠存活，并触发前缀空区复用/compact 后 capture offsets 仍正确。450 KiB 输入的源/目标重叠超过该共享 arena，按预算失败，不分给每条规则独立空间。信用转移不双计、pool 已 drop 时 clone 仍持真实存储、最后 owner 释放才可重用额度有独立 core 生命周期断言；原取消测试改用无界捕获输出，让被取消的真实 arena 与最后输出 clone 都参与 owner 生命周期。

### 本轮实际命令及更正

- `cargo check --locked --offline -q`：B 初次接入通过。
- `cargo test --locked --offline -q --lib body_transform:: -- --test-threads=2`：9 项通过；随后新增两项信用生命周期测试。
- 首轮 `cargo test --locked --offline -q --test body_transform -- --test-threads=2`：18 通过、原 H2 故障 fixture 700 KiB 因新 arena 能正确处理而不再失败；将超限源改为 1 MiB，保留无正常 EOS/RST、同物理连接兄弟正常及 trailers 断言，不删除危险用例。
- 首轮两个新增无界回归中，单 stage 边界通过，多 stage 因空输入错误占 region 位置返回 Finished；修正空输入不建立 live region、无输出不推进非 EOF 下级后两项通过。clippy 指出 batch regex 在 loop 内重复编译，移到 loop 外后通过。
- `cargo fmt --all && cargo check --locked --offline -q && cargo clippy --locked --offline --all-targets -q -- -D warnings && cargo test --locked --offline -q --lib --test body_transform -- --test-threads=2`：65 lib、21 body_transform 全通过；日志 `/var/tmp/rill007-arena-tests.log`。
- 末轮首次 fmt-check 指出新 stage 数量 guard 未格式化，后续命令未执行；fmt 后重跑 `cargo fmt --all --check && cargo check --locked --offline -q && cargo clippy --locked --offline --all-targets -q -- -D warnings && cargo build --release --locked --offline -q && cargo test --locked --offline -q --test configuration --test http_proxy --test https -- --test-threads=2`：全部通过，37/42/28 项通过；日志 `/var/tmp/rill007-arena-gates.log`。

最终将无界模式 gzip→zstd / zstd→gzip 小块回归也纳入后，连续重跑 `cargo fmt --all --check && cargo check --locked --offline -q && cargo clippy --locked --offline --all-targets -q -- -D warnings && cargo build --release --locked --offline -q && cargo test --locked --offline -q --lib --test body_transform --test configuration --test http_proxy --test https -- --test-threads=2`：全部通过（65 lib、21 body_transform、37 configuration、42 http_proxy、28 https）；日志 `/var/tmp/rill007-arena-final.log`。`git diff --check` 通过，暂存区为空，target 仍是原外部 symlink。

本轮未改 vendor、依赖、锁版本或 target symlink；父保存的 vendor.patch 无需因 B 更新。父独立复审与最终 nobody 全套/netns/gRPC 仍是验收关卡，不把修前证据当修后结果。

## 最终独立复审的新 P1：旧代理响应头兼容

独立 review `0b09c766` 已关闭此前三项，但指出新的 P1：先前统一最终头检查无条件执行，误把没有 chain/RequestPolicyState 的旧 proxy 也限制为策略域 64 KiB。原 handler 入口只对 `ctx.policy.is_some()` 检查；无策略的原框架响应头边界不应被正文准备出口收紧。修前 `suite/gates/netns/grpc.log` 中父报告的 285/0/1 等结果保留为修前证据，不作为此次修后通过。

- 先新增真实回归 `tests/body_transform/http.rs::legacy_proxy_preserves_upstream_headers_above_the_policy_limit`，旧 site proxy 从真实上游收到 80 KiB X-Pad、200 和完整 13 字节正文；客户端只在该用例中使用原 TcpStream 的有界 96 KiB 读取，不依赖可能更小的头 helper 门限。`cargo test --locked --offline -q --test body_transform legacy_proxy_preserves_upstream_headers` 修前确实失败为 503，日志 `/var/tmp/rill007-legacy-header-before.log`。
- 最小生产改动仅 `src/body_transform/response.rs:29,44,92`：Prepared 在 select 时记录 `policy.is_some()` 的 `policy_scope`；finish_budgeted 只在该域统一检查最终头。不以 selection/active 代替策略域，因此有策略但无正文声明、MIME 未命中、HEAD、406、仅 Vary、active/default 都继续检查。没有改 arena、vendor 或依赖。
- 修后同一真实回归返回 200，80 KiB X-Pad 和正文逐字节检查通过；同时单独重跑 `finalized_transform_headers_cannot_exceed_the_shared_header_limit`，策略域 active GET/HEAD/406/仅 Vary 的 64 KiB 门限仍通过。
- 执行 `cargo fmt --all --check && cargo check --locked --offline -q && cargo clippy --locked --offline --all-targets -q -- -D warnings && cargo test --locked --offline -q --test body_transform --test http_proxy -- --test-threads=2`：通过（22 body_transform、42 http_proxy）；日志 `/var/tmp/rill007-legacy-header-after.log`。本项当时尚待父源码复核及最终 nobody/gates/netns/gRPC 收口；最终结果见下节。

## 最终独立复审与父会话收口

最终独立 reviewer `c70bc4fc` 只读复核策略域修复，确认 `policy_scope` 冻结 `policy.is_some()`，最终头检查仍覆盖该域所有出口，旧无策略代理恢复原行为，结论 **OK / No issues found**。此前 identity 大小写、最终头检查及无界 arena 三项也已独立关闭。reviewer 没有运行命令，以下均为父会话实际运行证据。

最后修复后，父在 0755 工作区 `/var/tmp/rill007-final-suite-5icOxN` 重新同步当前 Cargo、src、tests、vendor、examples 和文档，使用既有外部测试缓存构建九个工件：

```sh
CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target cargo test --locked --offline --no-run --message-format=json
runuser -u nobody -- <each-test-executable> --test-threads=2
```

从 Cargo JSON 的 `profile.test=true` 选择并去重九个 executable，逐一执行，所有工件退出 0。没有放宽 `/root` 权限，没有更改仓库 target 符号链接。

| 顶层工件 | 通过 | 忽略 |
| --- | ---: | ---: |
| main | 0 | 0 |
| lib | 65 | 0 |
| configuration | 37 | 0 |
| http_proxy | 42 | 0 |
| static_files | 27 | 0 |
| https | 28 | 0 |
| reload | 34 | 1 |
| body_transform | 22 | 0 |
| feasibility | 31 | 0 |

合计 **286 passed、0 failed、1 ignored**，不重复累计隔离子进程摘要。唯一忽略项仍要求专用 `127.0.0.1:53` resolver，未擅自启用；此前 root/根目录不可访问造成的四项权限失败均在该非特权完整套件中通过。原始 [suite.log](data/021-body-transforms/suite.log) 包含全部顶层退出状态。

父在最后生产修复后另实际连续执行并通过：

```sh
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo build --release --locked --offline -q
cargo build --locked --offline -q
cargo test --locked --offline --doc -q
git diff --check
python3 tests/dynamic_proxy/netns.py
python3 tests/feasibility/grpc_container/run.py --rill --image 394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853
```

[gates.log](data/021-body-transforms/gates.log)、[netns.log](data/021-body-transforms/netns.log)、[grpc.log](data/021-body-transforms/grpc.log) 保存原始结果。namespace 矩阵 14 项 PASS，自动隔离仅 lo 的模拟公网/私有 resolver，无真实公网或 metadata 请求；主动断开空闲 TLS 的 Python BrokenPipe 堆栈照实保留，进程退出 0 且全部断言通过。真实生产 gRPC 回归六个场景通过：h2c/TLS h2、两者背压、两者旧流跨 CA/路由 Reload；输出 `passed=true`、`cleanup=true`、`cleanup_errors=[]`。只使用已缓存镜像及 `--pull=never`，临时容器、网络、服务进程和密钥清理成功。此回归验证同版 vendor 默认路径未退化，不冒称新增通用请求 trailers 能力。

修前 285 项全套及隔离日志保留为 `arena-{suite,gates,netns,grpc}.log`，不混作最后修复结果。真实失败证据保留为 [header-before.log](data/021-body-transforms/header-before.log)、[identity-before.log](data/021-body-transforms/identity-before.log) 和 [legacy-header-before.log](data/021-body-transforms/legacy-header-before.log)；前者含构造的长填充头，不含凭据。

父已将本地原 pingora-proxy 0.9.0 archive SHA-256 与原锁值核对一致，并逐文件比较 Cargo.toml/Cargo.toml.orig/LICENSE 和修改源，生成 [vendor.patch](data/021-body-transforms/vendor.patch)。锁文件原包名/版本集合没有变化，仅同版本 proxy 改为本地 patch 和新增已批准的直接依赖引用。新源码及证据无私钥材料或 emoji，用户 `AGENTS.md` 未修改。最终 release 为动态链接 x86-64 ELF，不声称全静态或已验证 Rust 1.95。

共享总额、codec/既有 HTTP 内部缓冲边界、字节匹配和条件请求例外均按本阶段用户明确确认记录于 CONTRACT。单规则 880 KiB 是该具体工作集的实测待决容量，不是新的通用正文限额。旧 error-page 不扩 DSL；错误链响应使用现有动作。T01–T03、AC01–AC03 与 G04 依实际代码、独立复审及上述运行结果关闭；不自动开始 008，不进行生产切换。
