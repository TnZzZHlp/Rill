# 022：受约束 WebDAV 验收记录

基线 `81da0483e6507c0869bdb0400f3779116604097c`。008 已经独立复审及父会话实际验收通过，T01–T03、AC01–AC03 与 G02 完成；完成提交以包含本记录的 Git 提交为准。本文保留各实施切片、工具时限失败与修复历史，其中旧“待验收”以第 11 节最终收口为准。未改 AGENTS、target 符号链接、观测功能或 009，未推送。工具任务超时与产品测试失败分别记录，不将旧检查点结果冒充当前完整验收。

**状态更正：** 上版 155 行记录的“Proof 未实施、退休/跨重启屏障未实现、三项工具尚未批准”已经过时。用户合并表单 `d4fd7a63` 已批准 rustix 1.1.4 安全 FD/xattr、持久私有 inode tag + O_TMPFILE 发布、最小 HTTP helper、独立 private mount namespace 的 1 MiB tmpfs ENOSPC fixture，以及 Debian 官方 cadaver/必要库的临时隔离准备。相应实现和真实客户端/ENOSPC 测试均已执行。仍不能把普通 FS 测试说成 casefold 专用 FS 或全部崩溃窗口已实测，独立代码复审、非 root 全套和最终验收仍 pending。

## 1. 模块与冻结接口

- `config/parser/chain_webdav.rs` 只解析 `webdav root="..." storage-id="..."` 终止动作，两个字段必需；复用鉴权 dominance，每条可执行路径须先成功 Basic/token。不扩 Profile/Snippet/error-page DSL。
- `http_policy` 保存同请求成功鉴权动作和冻结捕获，解释目标沿用同一快照、原方法/入口头/客户 IP，只纯计算路径和临时头副本，不重新验密码、发请求、重放副作用或猜 rewrite 逆映射。`proxy` 只接入终止分派与 Session 桥接。
- `webdav/{path,authorization,conditions,locks,xml}` 管资源键、同域授权、If/ETag、独占锁和 XML；`request_model/request/methods/put` 管头、流式桥接、业务和原子上传；`namespace` 管请求级目录项证明；`private_file/visibility/properties/registry` 管私有 inode、退休、属性和激活；`budget/limits` 复用既有 policy pool、磁盘信用、许可和取消。
- 公开低层接口保持 `StorageRegistry::prepare(identity, Arc<File>) -> DavResult<StorageCandidate>`、`StorageCandidate::activate(self) -> DavResult<ActivatedStorage>`。配置 adapter 保留真实 Origin/候选信用，Root 的 `from_prepared_fd` 复用安全能力验证，不借 proc 路径重新授权；candidate 绑定原 registry 的内部 Arc，不切换 global。
- check/prepare 不创建私有目录、属性、探测文件或活动 registry 项。完整候选先准备，fallible activate 持 RAII，再最终 gate、唯一 store、不可失败 commit；失败只按身份清理候选自己新建项，保既有属性/锁/FD。公开 `RuntimeState::new` 语义不变；server 内部 `new_deferred`/startup guard 跨运行时、watch、signal、listener、resolver、control 等准备，在任何请求 worker spawn 前才 commit。
- 相同 storage-id/root dev/ino 跨代复用锁和 namespace gate；移除后历史身份有界保留，换 inode 重加须重启。不同 ID 的同根/父子物理根（含历史）拒绝；同设备跨 mount 无法证明分离时拒绝，不建立平行锁域。重启丢内存锁但保属性。

## 2. 请求级 Proof、共享预算与变更

`NamespaceSession` 显式持 `Arc<StorageHandle>`、owned namespace write guard、`Mutex<Proof>`，由 DavRequest/Upload 贯穿实际 blocking 所有权；没有 thread-local、ambient 或跨请求路径缓存。父目录缓存以 dev/ino/mount/type 身份绑定，持真实 FD。每个已存在组件先从受约束父 FD 枚举精确 bytes 拼写，再受约束打开并比身份；没有精确项但文件系统折叠解析成功时返回 alias 拒绝，不把它当可创建的缺失。DAV 拒 symlink、普通文件 nlink>1、特殊资源/跨 mount；不扩大为一般静态文件全面禁链。

枚举用 `rustix::fs::RawDir`、固定 8192 字节栈缓冲与保存的目录 cookie，按需增量读取；正证可提前返回，缺失证明须枚举到尾。父 FD/index/container、名称和输出在分配前预留 policy pool 信用。请求仅一份 ledger：根预扣一项（remaining=4095），实际扫描所有非 dot 条目（包括私有名称）及新建绑定扣同一余额；不是每组件、每个 walk、source/target 各 4096，也没有额外 17 项。children/walk 复用缓存，不重置额度；深度上限 16。

主源、Destination、tagged If 的存在资源、GET handoff、PUT parent/commit 都经过该 session。内部 mkdir/remove/create/rename 精确更新相关父目录项及身份，不无脑 clear 全缓存重扫，也不在 namespace guard 释放后继续盲用。COPY/MOVE 的 source 和 overwrite 旧 target 共用 ledger；先完成两树枚举/逐项授权、COPY 源可读预检、旧树私有标记/身份复验，再删除旧 target。跨设备 MOVE 在删除前拒绝，不 fallback-copy。新增绑定也收费，所以 4096 是共同上限，不是任意形状操作必成功的保证。

新增 `copy_overwrite_source_and_old_target_share_one_enumeration_budget_before_any_delete` 真实 HTTP 回归验证两树合计超限时 503，source/旧 target 均保留。平铺 PROPFIND 边界修正为 4094 个可见文件 + root + `.rill-dav` 实际项达到 4096；新增一项失败，不提高预算。复杂 branch 的逐目标解释仍可能先用尽策略步数。

源规范 URI 单次解码后按 unreserved 规则重编码，堵 `%70rivate`/重复斜线挑选弱 auth；它不替代上述实际目录项证明。所有实际新增目标/子资源解释与主/错误链共用剩余 1024 步；仅严格闭合的首规则 unconditional/prefix + 已知 method/header/IP 布尔条件、同 handle、明确 strip-prefix 域可复用证明，不猜复杂 branch/rewrite/try-files 域。

## 3. 持久私有 inode 与普通 static

固定保留名为 `.rill-dav`、`.rill-dav-put-`。目录 0700/文件 0600 不被当作同服务 UID 静态入口屏障。`private_file.rs` 统一内部可读文件创建：

1. 同 parent FD 下 `O_TMPFILE|O_RDWR|O_CLOEXEC`、0600 匿名创建。
2. `fsetxattr(CREATE)` 写固定 `user.rill.webdav.private=rill-dav-v1`，sync 文件并核验正确标记。
3. 通过准备时已验证的自身 procfd、原 FD 持活的安全 link 发布保留名；核验原 inode、命名目标身份和 nlink。此处 procfd 不作为客户端路径授权。
4. owner、持存属性替换 temp、PUT temp 全部走此路径，没有“先可见再打标”的 fallback。activate 实际创建/解绑探测项、sync parent 并验证可清标记；O_TMPFILE/xattr/安全发布能力缺失则 fail closed。check 不执行写探测。

属性加载要求固定标记正确；owner 内容格式已提升为 `rill-dav-v2`（与上述固定 xattr 值不是同一概念），旧未 tag 检查点格式不接管。管理员须隔离/清理旧检查点数据，不能识别任意历史未标记硬链接。既有用户同名目录、归属/身份/权限/格式不合则拒绝，不自动整理。

所有静态 Root（包括旧快照、重启后仅 static 的新进程）在安全重开普通文件并比较 dev/ino/type 后、缓存/正文交付前，以固定 32 字节栈 buffer `fgetxattr`；标记存在或 ERANGE 拒绝，NODATA/明确 FS NOTSUP 视未标，其余错误（含 NOSYS）拒绝。此默认路径会增加一次固定 xattr 检查，不宣称所有文件系统均不受影响；普通 static 不因未支持 user xattr 而一概不可用，DAV activate 仍须真实支持。目录使用 O_PATH 祖先与固定 `.rill-dav` 子目录身份屏障，结合词法保留名、procfd 最终路径 deny 和活动私有身份检查；不为每个普通 static GET 增加目录枚举预算，不无理由要求 execute-only 目录 O_RDONLY，普通安全 symlink 规则不变。

`visibility.rs` 的 lease 持真实 File、引用计数、published 位、policy pool 信用和可选未释放 PUT 磁盘信用。PrivateEntry Drop 只原子退引用，不取 visibility 锁或在 Tokio 做阻塞 IO；每次读写入口最多回收 64 项。退休未发布 inode 只在 nlink0 才回收；硬链接仍在则保 FD/预算，避免 inode 重用误伤及提前放额度，不用永久裸 dev/ino tombstone。属性替换/取消上传留下 alias 仍保固定 tag，进程退出后也不会因无活动 registry 而可读；退休私有 inode 不清 tag。正常 PUT 完成才转 published、允许正常目标被读。

已通过真实取消 PUT alias、旧属性替换 alias、保留硬链接不先删除、重启仅 static、私有目录 symlink、缺失 owner tag 启动拒绝；单测验证 nlink0 才释放真实 FD/预算。未声称实际 casefold FS 亲测，未声称能抵御管理员剥 tag/复制私有内容或恶意挂载/文件系统。

## 4. 协议、取消与提交顺序

支持 OPTIONS、PROPFIND、PROPPATCH、MKCOL、GET、HEAD、PUT、DELETE、COPY、MOVE、LOCK、UNLOCK 的 Class 1/2 常用操作，不含共享锁、ACL、版本控制。GET/HEAD 复用既有 Range/条件/静态表示/007；写响应不做正文变换，安全/容量失败不进入错误链改成功，头后失败不发送第二响应。

- XML 1 MiB/深32；拒绝 DTD、未知实体、非法 XML，保扩展名、属性、混合内容与完整 namespace bindings/QName 值绑定。
- 属性每资源64项/规范XML合计64 KiB；私有JSON封装另有由这些界限推导的512 KiB防御上界，不扩大公开容量。PROPPATCH 临时完写/sync 后原子替换，活属性拒绝且整批不污染旧属性；替换后父 sync 失败可能已完成。
- 锁1024/storage、4096/process、最长3600秒；过期释放 owner/额度，token不写日志。If 的 tagged/untagged、AND/OR/Not、ETag/token 真值与提交语义分开处理。
- PUT256 MiB/file、1 GiB/process、并发16、总期限300秒、块最多64 KiB；实际写前预留。纯计量测试不冒称写满1 GiB磁盘。Upload在异步读与短blocking写间移交唯一所有权，网络等待不占blocking线程；取消清理持namespace/FD/许可/券到真正结束，未发布磁盘额度可转退休lease直到最后link删除。
- 64 MiB policy pool 计 XML/属性/历史/锁/Proof/存活输出等逻辑信用，不是进程 RSS 或文件系统容量保证。

PUT 顺序为临时内容 sync →父身份/条件/token/目标复核→temp nlink检查及预备 Rename→`Cancellation::admit_commit` CAS→无 await 的 rename→标记 committed→父sync→清固定私有xattr→file sync→visibility publish。长准备均在CAS前；CAS是commit准入仲裁，不是rename可见性线性化证明。取消先赢不rename，清自己的temp；准入先赢后的不可抢占syscall可跨过断开/期限。

rename失败清temp且保旧target；rename成功后parent sync或清tag/file sync失败返回 `DAV_COMMIT_DURABILITY_UNKNOWN` / `DAV_COMMIT_MARKER_UNKNOWN`，可能已提交但不可读，不删除新target假称回滚、不自动重试。清tag后file sync失败会尽力恢复tag/sync，但不声称恢复必成功。崩溃窗口须按unknown处理；管理员先确认目标完整性，再显式清固定xattr并fsync文件恢复可读性，没有新增恢复CLI。真实回归覆盖解绑temp导致commit失败保旧target、手工留下标记使static拒读、显式恢复后可读；不是syscall故障注入或每个崩溃窗口实测。

DELETE/COPY/MOVE 的数据与属性没有跨文件原子性，207按资源报告失败；目录复制/overwrite在枚举预检通过后仍可能部分完成（包括新增绑定耗尽剩余额度），不能从PUT原子性外推树事务。

## 5. 实际门禁与最后编辑核对

记录来源为本实施会话真实工具输出及续接摘要；未为未落盘的旧stdout虚构独立log文件。rustc/cargo实测均1.99.1，未测MSRV1.95。Cargo检查均使用 `--locked --offline`；长命令用 `set -e`/`&&`，不以末条成功掩盖前项失败。

上一轮综合检查点实际通过：

```sh
cargo fmt --all
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo test --locked --offline --lib -q -- --test-threads=2
cargo test --locked --offline --test webdav -q -- --test-threads=2 --skip webdav_file_and_parent_permissions
cargo test --locked --offline --test static_files -q -- --test-threads=2 --skip check_rejects_unreadable_root --skip cache_does_not_bypass_new_permission_denial --skip spa_does_not_fallback_for_denied_file
cargo test --locked --offline --test reload -q -- --test-threads=2 --skip watch_permission_denied_retries_are_bounded_and_chmod_recovers
cargo test --locked --offline --test feasibility -q -- --test-threads=2
cargo test --locked --offline --doc -q
cargo build --release --locked --offline -q
cargo fmt --all --check
git diff --check
git diff --cached --quiet
```

|检查|实际结果/范围|
|---|---|
|lib|74 passed；DAV定向core 9 passed（含退休FD/预算单测）|
|DAV|17 passed、2 explicitly ignored、权限1 filtered；20项总数，ignored分别为真实client和root mount，用下面单独命令实际运行，不等于未测|
|static_files|24 passed、3 root权限相关filtered|
|reload|33 passed、DNS1 ignored、权限1 filtered；一次symlink retarget ENOENT后原样定向及整套重跑通过，未改该测试|
|feasibility|31 passed|
|doc|0 tests，命令成功|
|release/check/fmt/clippy|上述综合检查点通过，不冒称每次后续小改均已重跑release/全套|

综合检查后，还实际通过 `cargo test --locked --offline --test configuration -q -- --test-threads=2`（37）、`--test http_proxy` 同参数（42）、`--test body_transform` 同参数（22，含内嵌CPU子进程检查）。追加创建绑定ledger预留、owner格式v2和request准备metadata证明后，又跑fmt/check、默认DAV（17/2ignored/1filtered）、all-targets clippy通过。

最后两项代码编辑为：permissions回归增加MOVE到只读目录403断言（当前root工作区尚未执行此权限项）；methods真实回归增加collection COPY、overwrite COPY、MOVE整树操作。随后 `cargo test --locked --offline --test webdav webdav_real_methods -q -- --test-threads=1` 实际1 passed，all-targets clippy通过。

末尾尝试的 `methods.rs` buffer精确替换没有命中文本，未产生改动，也不是待实现需求；`namespace.rs` 相同文本替换无变化。短暂试写的visibility重开表达式和namespace包装函数当场撤回；当前仍为持真实 `file.try_clone()` lease和现有 `validate_tags`，无那两段临时试写残留。architecture/configuration/CONTRACT三处最终同步已落盘，本次续接已核对；本轮优先修正此前落后的022，不据无操作edit宣称新增功能。

本轮收口实际执行 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`git diff --check`、`git diff --cached --quiet`，set-e全部通过，输出 `CHECKPOINT_CHECKS_PASSED`；核得19个tracked改动路径、36个untracked文件，stage空、HEAD仍为基线。另核最后试写残留不存在、三份同步文档无旧BLOCK/待授权断言。未重跑已通过长套件，不把本轮check成功夸称全suite/release重验。

## 6. 真实客户端与真实 ENOSPC

父准备的cadaver保留在 `/var/tmp/rill008-cadaver-z5tepO/root/usr/bin/cadaver`，必要库位于同root下 `usr/lib/x86_64-linux-gnu`；本轮已核对可执行路径仍在。未宿主安装/升级、未重复下载，父保留官方包来源/hash/ldd/smoke资料并负责最终清理。仅命令局部 `LD_LIBRARY_PATH`，每次临时0700 HOME/rc/netrc（netrc0600），不用真实用户配置。测试只访问loopback，fixture凭据不写入本文；输出lock token已脱敏。

以下两个ignored测试在上一轮**分别显式执行并通过，多次运行不是静默skip**：

```sh
cargo test --locked --offline --test webdav isolated_tmpfs_enospc -- --ignored --nocapture --test-threads=1
RILL_DAV_CADAVER=/var/tmp/rill008-cadaver-z5tepO/root/usr/bin/cadaver \
RILL_DAV_CADAVER_LIBS=/var/tmp/rill008-cadaver-z5tepO/root/usr/lib/x86_64-linux-gnu \
cargo test --locked --offline --test webdav real_cadaver_and_rclone -- --ignored --nocapture --test-threads=1
```

实际客户端版本cadaver0.26/neon0.34.2/readline8.2；`--version`退出255是已知版本行为，测试明确断言，quit/实际传输退出0。真实ls、MKCOL、PUT、属性set/get、LOCK/UNLOCK、COPY/MOVE、GET字节比较、DELETE/rmdir通过；不是手写HTTP冒充客户端。`/usr/bin/rclone v1.60.1-DEV`真实mkdir、upload/list/download/cmp、server-copy/move、delete/rmdir通过。最后显式重跑两项的外层命令返回成功，输出尾部受工具显示截断，不能补造逐行完整日志；完整工具结果归原会话记录。更早rclone独立检查点 `/var/tmp/rill008-rclone-checkpoint-jxlbpx` 早于最新Proof，不单独拿它证明最新实现。

`tests/webdav/disk.rs` 用unshare新mount namespace，比较父子namespace不同后执行 `mount --make-rprivate /`，只有命令成功才挂 `size=1m,nr_inodes=128,mode=0700` tmpfs。实际1 MiB fixture上传触发真实ENOSPC→507，旧target字节保留、temp清理，available bytes **1040384→1040384**；进程退出/umount完成。非root显式执行此fixture会报错，不会silent pass。没有SIGXFSZ/进程死亡冒充ENOSPC，也没有在父mount namespace挂载。源码当前验证private传播方式是检查make-rprivate命令成功，未另读mountinfo做独立交叉断言，留复审确认是否需补强。

最小 `tests/webdav/support.rs` 复用既有connect/head，4 MiB正文上限；支持CL/chunked、chunk扩展/尾部、HEAD/204/304无体，拒重复CL、TE+CL歧义、非法/截断分帧；未加新框架/mock/依赖。新测试覆盖退休/commit、007交接、编码名称/属性/COPY、遍历与共享步数、16个半上传、锁/属性/重载/启动RAII。

## 7. 依赖与失败/更正记录

新增直接依赖仅批准的 `quick-xml = "=0.38.4"`（MIT，default=[]）与 `rustix = "=1.1.4"`（default-features=false，fs/std，Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT）。必要新传递包仅linux-raw-sys0.12.1；memchr2.8.3、bitflags/errno/libc/windows-sys等均既有锁定版本。Cargo.lock只增加这三包及rill两依赖名，旧package无升级/移除，未cargo update、未联网取Rust包。已从本地缓存归档逐字节核解包源码：quick-xml41、rustix332、linux-raw-sys473文件，mismatches=[]；已读license，归档SHA与lock一致：

```text
quick-xml 0.38.4 b66c2058c55a409d601666cffe35f04333cf1013010882cec174a7467cd4e21c
rustix 1.1.4 b6fe4565b9518b83ef4f91bb47ce29620ca828bd32cb7e408f0062e9930ba190
linux-raw-sys 0.12.1 32a66949e030da00e8c7d4434b251670a91556f4144941d37452769c25d58a53
```

必要首次offline锁增量后全部Cargo检查使用locked/offline。quick-xml早期 `.cargo-checksum.json` 不存在，改核实际crate SHA/归档文件；不把缺文件命令报错算成功。

保留真实失败与更正：

1. 早期KDL fixture漏分号→CONFIG_SYNTAX，修后通过；早期clippy8项错误曾被外层末码掩盖，随后set-e独立失败/修复/重跑通过，不追认旧输出。
2. URN被http::Uri拒绝、href unreserved编码/根collection末slash、临时AsciiSet借用与若干clippy项均曾失败，改后core/原回归通过。
3. 早期rclone夹具control目录0755使服务拒启/客户端connection refused，改0700后真实互通通过，不算产品bug。
4. 真实16半上传曾WouldBlock：长期blocking_recv耗满16blocking线程；改Upload逐块短任务唯一所有权后同回归通过，未加线程或预算。
5. 接口偏离冻结草图、startup过早commit曾自检发现，改adapter/deferred RAII并补启动回归；不以此代替其他安全修复。
6. 共享Proof实施后旧平铺PROPFIND预期207实503，因私有扫描真实计费；把测试边界改为4094可见文件+root+metadata，未放大额度。
7. 最后request准备提前metadata后，一次MOVE relation回归预期403实409；将root/祖先关系拒绝提前到目标metadata之前，同默认DAV重跑17passed。
8. 一次reload_follows_retargeted_entry_symlink ENOENT；未改该测试，原样定向与整套重跑通过，按观察到的flake保留，未证明根因消失。
9. 原worker曾3600000ms、后7200000ms工具任务超时；父停止核对无残余writer/stagecommit并备份。这是任务时限失败，不是产品测试失败。最后一条日期/status复合工具被中断，不能算该命令整体通过；本轮重新独立核stage/状态。

## 8. 保存状态、真实剩余验证与父下一步

旧备份 `/var/tmp/rill008-worker-checkpoint-DX6IuA/{tracked.patch,new-files.tar.gz,state.txt}` 和父第二次超时备份 `/var/tmp/rill008-second-timeout-IgSRak/{tracked.patch,untracked.tar,status.txt}` 均保留，本轮已核目录存在。当前工作树含tracked Cargo/配置/http_policy/proxy/runtime/static_files/doc接口改动，以及untracked parser/WebDAV生产模块、tests/webdav和本文；普通git diff --stat不含untracked，复审不能遗漏。HEAD未变、stage空；不自动提交。

当前不再等待旧授权表，也不声称仍完全没实现Proof/tag。尚待：

- 父独立设计/代码复审：精确dentry负证/缓存内部变更、共享ledger、持久标记及static默认兼容性、退休lease/磁盘预算、激活回滚、unknown恢复顺序。
- 可访问0755临时工作区内非root完整正常suite（包括新增MOVE权限断言及所有既有权限项），父最终release/check/clippy/全套。当前/root下skip是明确未测，不当通过；隔离DNS fixture不存在时不启用其ignored项。
- casefold/Unicode折叠专用FS未实际挂载验证，只有精确原拼写/FD身份算法与普通FS回归；不能拿大小写敏感FS的ENOENT称折叠漏洞复现。
- 未实际注入每个rename/parent sync/clear-xattr/file sync失败或每个崩溃窗口；现有手工marker恢复/解绑temp回归不是这些故障的等价替代。xattr不支持/安全link缺失的所有FS组合也未逐一实测。
- 真实16槽/属性/目录预算与1 MiB ENOSPC已测；256 MiB/file、1 GiB/process有边界计量覆盖，不冒称实际磁盘满额写入。
- 本轮为30分钟收口而非再次无限实现，不新增未批准fixture/CLI/依赖。代码已有可构建检查点；复审发现的新缺口由父决定后续最小修改，不以本文提前宣告无安全缺陷或AC完成。

父下一最小步骤是基于当前工作树（含untracked）立即独立review，并在非root工作区跑完整suite；确认问题和门禁后再处理022最终状态、T/AC/G02/README及唯一完成提交。cadaver隔离目录留父清理，不在此删客户端或重取包。

## 9. 独立复审 dfc9bea3 后四项 P1 修复检查点（续接中，未提前验收）

父独立复审确认此前请求级 Proof、两树共享4096、退休FD/持久tag/跨重启、PUT unknown和startup交易已实质闭合，另外发现四项P1，不能因旧门禁通过忽略：

1. `namespace::constrained` 缺 NO_XDEV，internal bind mount同inode别名可建立平行锁/鉴权域。当前已给DAV逐组件打开及写重开加NO_XDEV，普通static不改；先前普通路径回归通过。用户表单 **b73c40ef** 已明确批准在现有已核独立private namespace/原1MiB tmpfs内追加目录/file各一次bind实测；本次续接尚未执行，不再等待授权，也未冒称通过。
2. 新私有目录/link命名后至fallible visibility登记前无清理owner，quota/FDclone失败可留孤项。当前`private_file::NamedFile`在link前备妥清理parentFD/name/identity/信用，probe/owner/PUT/property共用；新目录在登记前建立CreatedDirectory guard，移交ActivatedProperties后保精确owner身份。cleanup用fstatat+inode核验unlink，不需要新FD；不声称能消除外部unlink失败。隔离lib子进程真实耗尽policy额度回归已通过，覆盖新目录失败撤销、link后登记失败、旧属性保全/无temp及额度恢复。
3. PROPFIND Selection克隆扩展Name退券，missing/format再复制放大无信用。当前Selection保留原XML Node owner、借用名称，不再克隆names，不新增namespace长度/64请求name限制；输出为预留信用的64KiB块，在207前构造完毕，escape逐段写入，输出owner移交不复制。live/属性中间owner有额外持券，PROPFIND不再拿固定4MiB假覆盖全部。真实长namespace+64请求项反复503头前失败、正常65项207/额度恢复回归已通过；新增lib输出owner额度单测尚待修后运行。
4. tagged If只证明原编码URI后解码读metadata，可作条件ETag探测（未声称COPY/MOVE写绕过）。新增弱token普通OPTIONS+canonical/encoded private tag回归，**修前encoded实际200、期望403**；修后在任何tag metadata前href_from规范external同快照/已成功auth域证明，共享原1024步；同回归通过。

上次工具3600000ms任务超时，父核无残余writer/stage并备份 `/var/tmp/rill008-review-timeout-5nchjI/{tracked.patch,untracked.tar,status.txt}`；这是工具时限失败，不是产品测试失败。最后clippy实报sliced_string_as_bytes，已改`&text.as_bytes()[..amount]`但修后clippy尚未执行。本次续接先保存本节，随后完成批准bind及必要门禁，结果将在下节追加。父此前nobody10工件首轮除reload均过（lib74、DAV18+2ignored含权限），reload两ENOENT后原样两定向和完整34/0/1通过；父fmt/check/clippy/release/debug/doc、ENOSPC/client/006netns/005grpc通过均属**修前**结果，不冒称本轮修后已过，保留flake历史不归根。T/AC/G02仍未勾，无stagecommitpush。

## 10. 四项 P1 修后交审结果（本轮完成，仍待父独立闭环）

本轮只续接上述四finding，不重复客户端下载/归档核验、不新依赖/框架/driver、不改普通static挂载契约。最后sliced_string_as_bytes修正已实际通过all-targets clippy，不再pending。

### 10.1 精确实现与复现层级

- **P1-1 / internal mount alias：** `src/webdav/namespace.rs::constrained` 与 `writable` 使用 NO_XDEV；这是每个请求组件的安全打开钩子，不只MOVE末尾检查，包含目录和file bind。修后在 `tests/webdav/disk.rs` 现fixture按 **b73c40ef** 实测：先核父子mount namespace不同，make-rprivate成功后读mountinfo确认无shared/master/propagate_from，再挂原1MiB tmpfs；完成ENOSPC并停原服务后，再核隔离/private，目录及普通file各一次bind，启动受控第二服务。实证目录同dev/ino、两file别名同dev/ino且nlink1；两别名GET/PUT均409、原文件不变，第二服务停止，file→目录逆序umount，再umount tmpfs。没有在host namespace挂载，也没有使用未批准额外容量。此次真实证据是**修后拒绝**，没有临时撤销修复重跑修前bind攻击，不冒称修前实测200。
- **P1-2 / publish-before-cleanup：** `private_file::NamedFile` 命名前备好parent FD/name/identity/Memory，命名后任何登记失败自动按身份清理；owner、PUT、属性temp及probe共用。PUT/属性rename成功后才keep，保持已提交unknown不回滚；新目录CreatedDirectory→ActivatedProperties精确移交，保存owner身份无需Drop时新开文件；`namespace::remove_owned`用fstatat+精确dev/ino/type再unlink，不需新FD。`private_creation_owns_cleanup_before_registration_quota_failure`真实隔离policy pool耗尽测试通过：新目录登记失败无孤目录；已link文件登记失败无孤temp；已有属性quota失败旧记录保全、无proptemp；释放额度后store成功，最终可再次申请完整64MiB。没有生产fault-injection开关、没有把OOM abort/外部unlink失败声称可避免；未注入FD clone失败，源码owner覆盖该fallible点。
- **P1-3 / XML扩展名称信用：** `methods::Selection`保留原`xml::Node`及其券、迭代借用Name，无selection.names.clone/全names复制；移除此前未获冻结契约批准的requested-name64限制（持存属性64仍不变）。`PropfindBody`以预留64KiB Buffer和有券index持完整输出，escape逐段写，不造长namespace临时String；live/property/serialize中间存活期有预留信用，PROPFIND不再靠固定4MiB responsecredit。全部块在Header207前完成，预算失败仅发送Error503；发送时移动块owner而不再分配正文副本。真实HTTP用解码160KiB namespace（`&amp;`重复，原XML约800KiB、64请求名）反复3次503且无半207，随后8KiB namespace/65请求名207、全部缺失名404和原文件保全，证明恢复/无新增name数量限。隔离lib `propfind_expanded_names_and_output_retain_credit_until_their_owners_drop`直接证明扩展输出耗尽503，drop输出后XML owner仍占券，最后drop Selection可重新申请完整64MiB。此处未抓修前RSS/分配日志，不冒称修前无信用放大已做内存测量；问题由源码review确认，修后所有权/恢复有测试。
- **P1-4 / tagged If条件元数据探测：** `request_model::prepare`先map原tag，再parse规范external并href_from纯解释证明同资源/已成功auth域，之后才namespace metadata；共享Authorization原1024步。真实弱token普通OPTIONS+canonical private tag403、encoded `%70rivate` tag修前200（失败断言）、修后403；源资源常规GET仍正常、strong资源内容不变。正常tagged/untagged If和client锁/属性流程通过。这里是条件metadata/ETag域泄漏修复，不夸称已证明COPY/MOVE写绕过。

### 10.2 本轮实际命令与输出

以下按set-e串联实际通过；没有最后exit掩盖早期失败。本轮未跑权限项，不借父修前nobody成功替代修后权限；DNS ignored未开启。

```sh
cargo fmt --all
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo test --locked --offline --lib -q -- --test-threads=2
cargo test --locked --offline --test webdav -q -- --test-threads=2 --skip webdav_file_and_parent_permissions
cargo test --locked --offline --test webdav isolated_tmpfs_enospc -- --ignored --nocapture --test-threads=1
RILL_DAV_CADAVER=/var/tmp/rill008-cadaver-z5tepO/root/usr/bin/cadaver \
RILL_DAV_CADAVER_LIBS=/var/tmp/rill008-cadaver-z5tepO/root/usr/lib/x86_64-linux-gnu \
cargo test --locked --offline --test webdav real_cadaver_and_rclone -- --ignored --nocapture --test-threads=1
cargo test --locked --offline --test static_files -q -- --test-threads=2 --skip check_rejects_unreadable_root --skip cache_does_not_bypass_new_permission_denial --skip spa_does_not_fallback_for_denied_file
cargo test --locked --offline --test reload -q -- --test-threads=2 --skip watch_permission_denied_retries_are_bounded_and_chmod_recovers
cargo fmt --all --check
git diff --check
git diff --cached --quiet
```

实际结果：lib **76 passed**（含两个隔离quota子进程）；DAV **18 passed / 2 ignored / 权限1 filtered**（21项总数，两个ignored随后分别显式通过）；static **24 passed / 权限3 filtered**；reload **33 passed / DNS1 ignored / 权限1 filtered**。本轮reload没有观察到ENOENT，不归因或抹去父此前两ENOENT历史。修后独立完整非root/最终release等仍留父，不冒称父责任已完成。

本轮完整可见fixture输出摘录（来自工具输出，未另造历史log路径）：

```text
isolated tmpfs size=1048576: status=507 old_target_preserved=true temp_cleanup=true available_before=1040384 available_after=1040384
isolated bind aliases: directory_same_identity=true file_same_identity=true file_nlink=1 DAV_GET_PUT=409 original_preserved=true
cadaver 0.26
neon 0.34.2: Library build, IPv6, libxml 2.9.14, zlib 1.3.1, GNU TLS 3.8.9.
readline 8.2
rclone v1.60.1-DEV
cadaver/rclone: loopback methods, properties, locks and byte comparison passed
```

cadaver真实ls/MKCOL/PUT/propset/get/LOCK/UNLOCK/COPY/MOVE/GET字节对比/DELETE/rmdir，rclone真实mkdir/upload/list/download/server-copy/move/delete/rmdir全部成功；临时HOME/rc/netrc、命令局部LD_LIBRARY_PATH、loopback、脱敏规则保持，未重复下载/宿主安装，客户端目录留父清理。

### 10.3 改动边界与交审

四P1生产改动路径：`src/webdav/{namespace,private_file,properties,put,request_model,methods,budget,xml}.rs`；测试更新 `tests/webdav/{methods,capacity,disk}.rs`，另两个隔离quota单测位于生产模块的cfg(test)。用户文档同步仅本022、architecture/configuration（明确NO_XDEV不改static、tagged规范鉴权、PROPFIND预算/头前失败、RAII）。Cargo/vendor/deps本轮未改，T/AC/G02未勾，无stage/commit/push。

此处为 worker 当时的待独立复审交付，不是 worker 自签安全验收；父后续闭环和最终运行结果见下一节。casefold 专用 FS/全部崩溃窗口仍未逐一实测，unknown 恢复边界不变。已批准两 bind 不再 pending，不把工具时限失败算产品失败。

## 11. 最终独立复审与父会话验收

独立 reviewer `714a95bd` 只读复核全部四项修复及直接回归，结论 **OK / No issues found**：逐组件 NO_XDEV、命名前完整清理所有权、PROPFIND 名称/输出全生命周期信用、tagged If 规范授权均已关闭。此前请求级 Proof、共享遍历、退休 FD/持久标记、跨重启、PUT unknown 和启动/Reload 事务也已由独立 review 确认。reviewer 未运行命令，以下由父会话实际执行，不用文档或测试数量代替源审。

### 非 root 全套

最后生产修复后，父把当前 Cargo、src、tests、examples、vendor、docs 与 writing-block.md 复制到 0755 工作区 `/var/tmp/rill008-final-suite-sJOE68`，没有放宽 `/root` 权限。使用既有独立测试缓存构建，从 Cargo JSON 选择并去重十个 `profile.test=true` executable，再逐个以 nobody 运行：

```sh
CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target cargo test --locked --offline --no-run --message-format=json
runuser -u nobody -- <each-test-executable> --test-threads=2
```

| 顶层工件 | 通过 | 默认忽略 |
| --- | ---: | ---: |
| main | 0 | 0 |
| lib | 76 | 0 |
| configuration | 37 | 0 |
| http_proxy | 42 | 0 |
| static_files | 27 | 0 |
| https | 28 | 0 |
| reload | 34 | 1 |
| body_transform | 22 | 0 |
| webdav | 19 | 2 |
| feasibility | 31 | 0 |

合计 **316 passed、0 failed、3 ignored**，所有工件退出 0，不重复累计隔离子进程摘要。两个 DAV ignored 在下一段分别显式执行通过；实际未启用的只有要求专用 `127.0.0.1:53` 的历史 DNS fixture。所有权限回归，包括 DAV 文件/父目录/MOVE 到只读目录，均在该非特权完整套件通过。[suite.log](data/022-webdav/suite.log) 保留全部顶层退出结果；未替换仓库外部 target symlink。

首轮修前父全套有两项 Reload ENOENT：failed_reload_preserves_state_and_generation、reload_follows_retargeted_entry_symlink。未修改测试，原样定向各一次及整套 34/0/1 重跑通过；此后上表最终全套也通过。保留 [initial-suite.log](data/022-webdav/initial-suite.log)、[reload-targeted.log](data/022-webdav/reload-targeted.log)、[reload-rerun.log](data/022-webdav/reload-rerun.log)。既有 Fixture::start 只等待 TCP 而不等控制 socket 是源码观察，尚未证明为本次根因，不声称已修复该 flake 或首次全过。

### 最终质量门禁、真实客户端及隔离错误

父在最后生产修复后连续执行并通过：

```sh
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo build --release --locked --offline -q
cargo build --locked --offline -q
cargo test --locked --offline --doc -q
cargo test --locked --offline --test webdav isolated_tmpfs_enospc -- --ignored --nocapture --test-threads=1
RILL_DAV_CADAVER=/var/tmp/rill008-cadaver-z5tepO/root/usr/bin/cadaver RILL_DAV_CADAVER_LIBS=/var/tmp/rill008-cadaver-z5tepO/root/usr/lib/x86_64-linux-gnu cargo test --locked --offline --test webdav real_cadaver_and_rclone -- --ignored --nocapture --test-threads=1
python3 tests/dynamic_proxy/netns.py
python3 tests/feasibility/grpc_container/run.py --rill --image 394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853
git diff --check
```

- [gates.log](data/022-webdav/gates.log)：格式、check、全目标零警告 Clippy、release/debug、doc 均通过。实际 rustc/cargo 1.99.1；release 是动态链接 x86-64 ELF，不声称全静态或 MSRV 1.95 已验证。
- [enospc-bind.log](data/022-webdav/enospc-bind.log)：父显式执行 root 所需 fixture，核独立 namespace/private 传播，仅在原 1 MiB tmpfs 内挂载。真实 507、旧目标保全、temp 清理、可用字节 `1040384→1040384`；目录/file bind 同 dev/ino 且 file nlink1，DAV GET/PUT 均409、原内容保留；逆序卸载并退出。没有宿主挂载、DNS 或权限变更，没有用 SIGXFSZ 冒充磁盘恢复，也没有因 nobody 缺权限静默通过。
- [clients.log](data/022-webdav/clients.log)：父显式执行真实 cadaver 0.26/neon0.34.2/readline8.2 与 rclone1.60.1-DEV，只访问临时 loopback Rill。方法、属性、LOCK/UNLOCK、COPY/MOVE、下载字节比较及清理全部成功；不是手写请求替代客户端。
- [netns.log](data/022-webdav/netns.log)：原动态代理隔离矩阵 14 PASS。主动关闭空闲 TLS 的 Python BrokenPipe 堆栈照实保留；进程退出 0、所有断言通过，不隐瞒为零异常输出。
- [grpc.log](data/022-webdav/grpc.log)：原生产 h2c/TLS h2、两者背压及两者跨 Reload 旧流六场景全部通过，`passed=true`、`cleanup=true`、`cleanup_errors=[]`，只用已缓存镜像和 `--pull=never`，临时容器/网络/进程/私钥已清理，不操作生产 Caddy。

两个显式 DAV ignored 用例均返回1 passed，子 namespace 再打印的同名摘要不重复计数；它们未改变历史 DNS ignored 的环境要求。修前 gates/client/ENOSPC/netns/grpc 日志保留为 `initial-*.log`，不混作上述最终结果。

### 来源、临时工具与交付边界

cadaver 仅下载官方 Debian 包 `cadaver=0.26+dfsg-2` 和 `libneon27t64-gnutls=0.34.2-1` 到临时目录解包，无宿主安装、apt upgrade、镜像拉取或重复下载。来源、版本和 SHA-256 见 [cadaver-source.txt](data/022-webdav/cadaver-source.txt)、[cadaver-packages.sha256](data/022-webdav/cadaver-packages.sha256)、[cadaver-download.log](data/022-webdav/cadaver-download.log)、[cadaver-version.log](data/022-webdav/cadaver-version.log)。`--version` 打印正常版本但退出255；无网络 quit smoke 和实际互通均退出0，不将其当下载失败。临时 HOME/rc/netrc 与 fixture 凭据已清理；父在最终验收后校验两包 SHA 并删除整个 `/var/tmp/rill008-cadaver-z5tepO`，故上面路径是实际历史命令，不是持久安装位置，复现须重新按已批准来源准备。

父独立核对 Cargo.lock：仅新增已批准 quick-xml0.38.4、rustix1.1.4、linux-raw-sys0.12.1 三包，所有旧包名/版本保留；从已知本地缓存 archive 计算 SHA 与 lock 全部相同，见 [crates.sha256](data/022-webdav/crates.sha256)。没有网络获取 Rust crate、cargo update 或无关依赖升级；未改现有 vendor。用户 AGENTS 保持不变，新源码/证据不含真实密钥、凭据或 emoji。

T01–T03、AC01–AC03 与 G02 依实际代码、独立复审和运行证据关闭。已批准机制、root/文件系统能力限制、私有标记恢复及未知提交结果必须按配置文档部署；casefold 专用 FS、全部 syscall/崩溃窗口未逐一实测，不宣称超出证据的保证，也不提供抵御受信管理员剥标记/复制内容或恶意挂载的承诺。不自动启动 009，不进行生产切换。
