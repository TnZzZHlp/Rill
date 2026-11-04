# 010：静态文件可选增强验证

## 范围与环境

基线为 `8b3b628aae453643a13e95e96914552808630e84`。保留其后的既有三目标报告，不将不同环境结果相减归因；未实施 011/012。

用户在访谈 `66de2a00-3e1a-49b9-a5b9-07d6ad3804d7` 批准预压缩、弱验证器、小正文缓存、错误页、目录浏览、nix `dir` feature，以及针对性三重复、每轮 30 秒 A/B。随后明确回复“批准”，同意直接声明已有锁定包 `bytes = "=1.12.1"`。锁文件仅 rill 的直接依赖列表增加 bytes，没有新增包或版本升级，未安装工具或拉取镜像。

Sub Agent 按配置、表示、错误页/浏览、缓存和文档分段实施；共享仓库始终单写入者，独立只读复审，父代理负责冻结源码、隔离执行和提交。

所有项目构建、测试、示例与服务均运行于 OS 沙箱：源码、工具链及 registry 只读，离线 Cargo，私有 mount/PID/网络命名空间，网络仅 loopback；UID 65534、GID 0、清附加组、capability 为零、清空继承环境。实际 Rust/Cargo 1.99.1，不声称最低 Rust 1.95 已动态验证。

- 质量门禁：CPU affinity 6,7，jobs/link/test threads 2，scratch 4 GiB，地址空间 2 GiB、CPU 300 秒、nproc 64、nofile 256、文件 512 MiB、wall 600 秒；仅隔离网络中允许所需的 DNS 测试低端口。
- release：affinity 8,9，scratch 8 GiB、地址空间 3 GiB、CPU 900 秒、wall 1800 秒，其余能力限制保持。
- A/B：服务 affinity 4,5，客户端 6,7，C8 闭环；scratch 8 GiB、地址空间 2 GiB、CPU 900 秒、nproc 128、nofile 512、文件 512 MiB、wall 1800 秒。没有公网流量、容器或外部上游。

## 实现与检查

### 表示与验证器

`representation.rs` 使用请求头原始字节处理编码、重复项最低 q 和非法已识别项 q=0，只裁剪 SP/HTAB。默认关闭，使用已有且不早于源文件的 `.br`/`.gz`，原文件必须先安全打开；全部表示不可接受时 406。MIME 取原路径，Range 针对实际表示字节，HEAD 忽略 Range。弱 ETag 独立区分表示，If-None-Match 优先日期判断，弱标签不能满足 If-Range 的强比较。

复审修复了非 UTF-8 参数绕过显式拒绝、Unicode 空白误当 HTTP OWS、相关响应缺少 Vary 等问题。typed HeaderValue 回归覆盖 NBSP 位于 q 值、参数名和编码名；验证器也拒绝非法非 OWS 前缀。Vary 的覆盖不被扩大描述为所有提前产生的 400。

### 有界缓存

`cache.rs` 与运行时绑定负责进程共享正文预算：默认 0，最大 256 MiB，单项不超过 64 KiB，ready/loading 共 1024 个槽位，LRU；同键加载中的其他请求直接 streaming。配置预算为启动项，变更返回 RESTART_REQUIRED。check 和候选资源准备不创建缓存池。

每次先安全打开，再用 namespace、配置代次、route、root 身份、实际路径、编码、dev/ino、长度、mtime/ctime 纳秒、mode、uid/gid 隔离键；命中重新 fstat，加载前后检查版本，变化或短读在发送响应头前拒绝。替换、删除、权限拒绝及 root Reload 不返回旧缓存。错误页、目录列表和无正文响应不缓存。

`Bytes::from_owner` 将预算许可与正文最后引用绑定，Range 使用 slice 而不复制。索引淘汰、pool 销毁或请求取消不能提前归还仍被正文/真实 blocking closure 持有的额度。registry 对 Budget 和 CachePool 均弱持有，正文仅强持 Budget，避免引用环；旧 pool 消失但正文仍活时同预算复用 Budget、不同预算拒绝。全局 namespace 不因 pool 重建而重置。

核心测试直接检查持有旧正文后的预算、pool 销毁/重注册、取消清理、单键加载、槽位上限和版本/编码/namespace 隔离。`RuntimeState::static_cache_stats()` 为只读库接口，不是新的 HTTP、控制或 CLI 端点。预算约束正文分配，不是 RSS 或流式缓冲总量；元数据、分配器和框架开销另计。不承诺恶意原位写入的事务快照。

复审发现缓存 hit 缺少第二次 FD 版本核验，已修复。`cache_hit_rechecks_opened_file_version` 先预热，再捕获 metadata 后 chmod，断言实际 `hits == 1` 且 ESTALE。隔离红色变体只移除命中核验：测试实际失败，退出 101；修复后的完整门禁通过。

### 错误页与浏览

错误页仅用于静态响应头发送前的指定状态；独立父目录 Root，叶文件失败或超过 1 MiB 时空体保留原状态及必要 Allow/Content-Range，不递归、SPA、压缩、缓存或条件验证，HEAD 无体。

浏览默认关闭，index 优先；稳定目录 FD、条目前后身份验证、HTML 转义和逐字节 href 编码。扫描 4096、展示 512、HTML 256 KiB，超限 503 而不截断。隐藏 dot、越界、特殊或不可安全访问目标；不提供文件 ETag/日期/Range，仅合法唯一 If-None-Match `*` 可做存在性验证。示例明确 site 级浏览会公开整个 root 下可访问无索引目录的名称与条目。

复审关闭目录枚举与 root 校验对象混淆；挂载子目录允许受核验的 mount ID 差异，不能仅因 readdir inode 与挂载根 inode 不同而误拒绝。最终在私有 mount namespace 中实际创建同设备 bind mount 和 tmpfs，非 root/cap0 服务对父/子列表和两种文件内容均验证通过，正常退出并删除 Socket。

## 实际门禁与示例

执行器在 `/var/tmp/rill-010.evlQY4/`，命令均由父代理在外层 `unshare --mount --net --propagation private` 中调用，内层按上述 bwrap/setpriv/prlimit 约束执行：

```sh
bash run-quality.sh source-formatted
bash run-hit-regression.sh source-cache-red
bash run-mount-check.sh source-release
bash run-release.sh source-release
bash run-static-ab.sh pilot 2 1
bash run-static-ab.sh full 30 3
```

红色回归预期非零；其余最终执行均退出 0。质量门禁内实际命令为：

```sh
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo test --locked --offline --no-fail-fast -q -- --include-ignored --test-threads=2 --nocapture
cargo build --release --locked --offline -q
```

159 项通过：core 34、configuration 25、feasibility 7、HTTP 19、HTTPS 20、Reload 28、static 26，零失败、零忽略。最终挂载执行再次通过 check/clippy/159 项测试。冻结源码与仓库 Rust 文件逐字节核对一致。

release 为 19,232,808 bytes，SHA-256：

`d829946dad524b9146060fcb2ef60b037c33a3e592402ea3b66590f1eefc01f5`

新示例先 check 原配置，再在 scratch 中仅覆盖安全控制 Socket 路径运行。真实验证 index、目录存在性 304、404 GET/HEAD、有效 identity/gzip/br 的正文、Range、HEAD 与独立 ETag，以及缓存后的替换/删除、正常退出和 Socket 清理。有效 sidecar 由沙箱内既有 libbrotlienc/libbrotlidec 与 Python gzip 生成并解码核对：1024 个 `s`，gzip 29 bytes、br 11 bytes；不是仅设置 Content-Encoding 的假压缩。

验证过程另保留了源码副本权限、fixture 目录权限及 rustfmt stdout 路径前缀捕获错误日志；分别修正只读副本权限/可读 fixture 位置/格式捕获脚本后重跑通过，没有把执行器错误当成产品测试通过。

## A/B 测量与限制

首次 pilot 为独立 7 轮、每轮 2 秒；不纳入正式结果。正式矩阵为三重复，每轮预热 2 秒、测量 30 秒：同一 release 的缓存关闭/16 MiB 开启 × 1 KiB/16 MiB 共 12 轮，缓存关闭下 identity/gzip/br 共 9 轮。顺序 AB/BA/AB，全部 21 轮服务正常退出、无强杀，驱动检查无 Socket 残留。

测量脚本复审修复了 client 把服务子树计入资源、复用连接丢 connection ID、窗口边界没有原始元数据的问题。client 使用 pid-self，server 保留完整进程树；每轮有独立 window JSON。

初次正式矩阵的独立逐行核验发现 1 条请求起始时间戳晚于 deadline 约 104 ns：旧 worker 先检查时钟再另取 begin。保留原始整批于 `artifacts/full-window-boundary-before/`，未删样本或悄悄改 summary。两个 worker 改为同一次 begin 采样先判断 deadline，再发起请求；随后完整重跑 21 轮，最终原始结果在 `artifacts/full/`。这里的 begin 是驱动时间戳，不是网络实际发包时刻。

最终 21 轮共有 8,106,101 个测量请求。三轮结果如下；延迟单位 ms，RPS 为窗口内完成数除以 30 秒，不是服务端容量。

| 场景 | p50（三轮） | p95（三轮） | p99（三轮） | RPS（三轮） |
| --- | --- | --- | --- | --- |
| 1 KiB cache off | 0.500 / 0.404 / 0.408 | 0.643 / 0.534 / 0.568 | 0.816 / 0.689 / 0.717 | 15117 / 18745 / 18484 |
| 1 KiB cache on | 0.403 / 0.416 / 0.405 | 0.550 / 0.573 / 0.551 | 0.702 / 0.705 / 0.732 | 18712 / 18122 / 18615 |
| 16 MiB cache off | 68.044 / 67.632 / 72.007 | 77.292 / 75.203 / 81.960 | 83.983 / 84.037 / 90.228 | 116 / 117 / 109 |
| 16 MiB cache on | 67.586 / 70.007 / 69.080 | 76.732 / 78.365 / 78.485 | 85.460 / 87.491 / 84.488 | 117 / 113 / 114 |
| transfer identity | 0.403 / 0.412 / 0.410 | 0.568 / 0.583 / 0.605 | 0.716 / 0.712 / 0.732 | 18659 / 18183 / 18087 |
| transfer gzip | 0.422 / 0.425 / 0.420 | 0.601 / 0.563 / 0.558 | 0.745 / 0.733 / 0.738 | 17885 / 17842 / 18095 |
| transfer br | 0.435 / 0.422 / 0.423 | 0.644 / 0.589 / 0.581 | 0.773 / 0.765 / 0.743 | 17138 / 17931 / 17898 |

缓存的小文件收益不跨三轮稳定，默认继续为 0；不据此扩大启用或宣称优化收益。大文件超过单项阈值，仍走原 streaming，观察波动不归因于正文缓存命中。transfer 与 identity A/B 使用不同正文校验逻辑，不能交叉当作缓存对照。预压缩确实将该高度重复正文从 1024 bytes 减至 29/11 bytes，但这不是一般压缩比或吞吐提升承诺。

全部 21 轮触发 client warning（峰值 CPU 达单核 90% 或在线 lag 达 100 ms），因此存在客户端限制。CPU 为采样值，RSS 求和可重复计算共享页，HWM 是历史值；无 PSS/cgroup 内存结果，不把 RSS 当正文预算。基准只记录配置预算，没有运行时命中率/实际 used 采样；额度与生命周期由核心测试验证，不伪称测量到了命中。encoded payload bytes 不含 HTTP/TCP 开销。

独立原始核验已完成：最终 110 个文件共 161,841,929 bytes 的 SHA-256/大小及完整清单一致；8,106,101 行全部满足 measure_start ≤ begin < deadline，无错误、正文大小与连接序号一致；nearest-rank 分位数、样本/正文总量、窗口完成数、late/drain/cohort 及资源采样峰值逐项复算与 summary 一致。pilot 40 文件及修复前整批 manifest 另行核对，不混入最终数据。具体计算结果见 `data/010/independent-raw-check-final.json`。在线 warmup 边界、重连与 loop lag 诊断不能仅由测量 CSV 重建；退出/Socket 清理结论结合驱动判定与日志，不冒充对全部时序的独立观测。采样和静态复审均不构成全时序证明。

## 证据位置

仓库保留小型摘要、原始 manifest、窗口与资源 CSV、最终执行日志及独立核验结果于 `docs/validation/data/010/`；逐请求 gzip 和完整配置/日志独立保留于 `/var/tmp/rill-010.evlQY4/artifacts/`，不将 162 MB 原始文件写入 Git。保留的数据需持续归档，Git 摘要不替代其存在性。

主要日志：`010-formatted-quality.log`、`010-cache-hit-red.log`、`010-final-mount-check.log`、`010-release-example.log`、`010-static-ab-full.log`。新上下文最终实现复审为 No issues found / Merge verdict OK；原始数据复核独立通过。最终验收与提交状态同步至 010 子计划；此记录不授权后续阶段。
