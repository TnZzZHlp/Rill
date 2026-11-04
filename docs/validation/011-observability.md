# 011：日志、指标与可选追踪验证

## 范围与环境

基线为 `ecca04df85d969ec716cd1e3999242694f08a06f`。用户批准默认有界实现、必要官方 SDK 依赖、Collector 0.133 和五模式三重复测量；随后批准四个环境变量 presence 检查，包括 off，以及直接声明 `opentelemetry-http` 的有界 HTTP 适配。后者声明时没有新增锁包，不代表整个 011 没有新增包。原有 writing-block 和 bench-three-way 报告未修改；012 未实施。

直接依赖均关闭默认 feature：`opentelemetry = 0.31.0`（trace）、`opentelemetry_sdk = 0.31.0`（trace）、`opentelemetry-otlp = 0.31.1`（trace、http-proto、reqwest-blocking-client）、`reqwest = 0.12.28`（blocking）、`opentelemetry-http = 0.31.0`（无额外 feature）。锁文件新增 70 个必要包，既有版本及来源校验和保持；仅补取缺失的官方 `opentelemetry-http 0.31.0` 归档 17,959 bytes 和 `tonic-prost 0.14.9` 归档 7,912 bytes，哈希与锁文件一致，非目标三个归档未获取。独立 registry 后验大小 868 MiB，不是配额上限。依赖记录见外部阶段目录 `/var/tmp/rill-011.nfYINU/dependencies/`。

官方 Collector archive 37,846,204 bytes，SHA-256 `4e8a9dc074d86c312ef8608f5a1c89b66d97003e7c03d58e0d3dc1f8897e317e`；普通 `otelcol` 二进制 135,585,976 bytes，SHA-256 `24b4f75796d6734f402d65f7a91cd102dead29a0156aa3d3428e277b8d9a056e`。只提取经验证的根目录普通文件，不使用 extractall 或跟随符号链接；848 bytes README 跳过。官方同源 checksums 匹配不是签名或独立信任证明。version 和配置 validate 在沙箱实际运行，见 `artifacts/011-collector-{version,validate}.log` 及 `collector/` 清单。

实际 Rust/Cargo 1.99.1，edition 2024；最低 1.95 未动态验证。执行使用私有 PID、mount、仅 loopback 网络，源码、工具链、registry 只读，离线 Cargo，空继承环境、scratch home、UID 65534/GID 0、无附加组及 capability。质量门禁 affinity 6,7，jobs/link/test 线程 2，scratch 4 GiB、AS 2 GiB、CPU 300 秒、nproc 64、nofile 256、文件 512 MiB、wall 600 秒；仅隔离 DNS 测试允许 UDP 53。原生 Collector 使用 Go AS 8 GiB、GOMEMLIMIT 128 MiB、GOMAXPROCS 2，Cargo 子进程 AS 2 GiB。release affinity 8,9、AS 3 GiB、scratch 8 GiB、CPU 900 秒、nproc 64、nofile 256、文件 1 GiB、wall 1800 秒，smoke 服务 AS 2 GiB。运行探针 scratch 512 MiB、wall 180 秒、CPU 120 秒、nproc 128、nofile 512、文件 32 MiB，Rust AS 2 GiB、Go AS 8 GiB。AS 不是 RSS 限额。

## 实现边界

配置和架构细节以 `docs/configuration.md`、`docs/architecture.md` 和源码为准。默认 off 不代表 owner 线程零开销。共享 P0 和 access 日志使用非阻塞 4096 × 4 KiB 队列，try-lock/try-send 在满、关闭或竞争时丢弃，由唯一 writer 输出；关闭等待 1 秒，不能取消已阻塞的 stderr。

指标永久保留最多 128 个 route，另有 unknown 和 overflow，最多 130 个 route、2162 个 series，限额不超过 1 MiB；失败候选不占槽，标签不含 generation 或原始输入。duration 使用 13 个互斥桶，输出为累积前缀、`+Inf` 与 count。端点仅 literal loopback 的 GET `/metrics`，16 连接、请求头 8 KiB、读写各 2 秒；没有应用层认证，本机其他用户可能抓取，仍依赖 OS 隔离。

官方 SDK/exporter 使用本地 root，丢弃外来 trace headers，只对采样的本地请求注入；不记录原始秘密 URL、IP、header 或 Host。这不意味着 P0 管理路径诊断已去地址或绝对匿名。采样是周期 counter 而非概率随机，opaque ID 不承诺加密匿名。access 使用实际状态，正文长度未知为 `None`。SDK 使用自定义公开 `SpanProcessor`，不是 SDK BSP：总许可 2048、batch 128、超时 2 秒、关闭 3 秒；HTTP 请求体限 1 MiB、响应体限 64 KiB，发送额外复制最多 1 MiB；这些限额不等于 RSS 限额，存活请求 span 不计入队列预算。SDK 默认可能读取环境，但有效配置显式覆盖且内部日志禁用，不设 global provider、不 unsafe 修改环境。check 不初始化 SDK，不能声称 check 执行了四环境变量拒启；P0 上游 `SSL_CERT_FILE`/`SSL_CERT_DIR` 行为不变。

## 质量门禁、失败与修复

基线 159 项；source-tracing-checked-uri 的 fmt 尚待统一，check/clippy 与 175 项通过。最终首次完整质量门禁 175 项中 174 通过，静态合法符号链接正文为空且未捕获 headers，保留 `artifacts/011-final-quality-collector-before-static-fix.log`，不能仅由该失败断定 errno。外部诊断保持同一路径及合法符号链接不变；固定 2000 次请求与无关 rename 并发 49134 次时 1981 次 200、19 次 403，19 次 `openat2` errno 11；前后安静阶段各三次 200。

修复仅针对 Root 的 EAGAIN，在同 root/path 上最多重试 8 次并 yield，保留 O_PATH、BENEATH、NO_MAGICLINKS，不回退。现有集成测试增加 1000 次合法 GET、rename 前进及清理，原逃逸 403 检查保留。旧代码红测退出 101，in-root index 返回 403；修复后 fmt check、check、clippy `-D warnings` 和完整 176 个不同测试全部通过。原生 profile 的专项重复不算第 177 项。证据为 `artifacts/011-static-red-green-quality.log` 和 `artifacts/quality-collector-static-fix/`；有限 8 次重试不保证持续竞争下所有请求成功。执行器独立核验四个 leaf SHA，见相应产物。收尾检查将 `cfg(test)` 的四字节 UTF-8 样本改为非 emoji 的四字节 CJK 转义；仅该测试输入变化，生产代码前缀逐字节一致，见 `final-test-fixture-delta.json`。最终源码快照为 source-final-v3，release/测量保留真实 source-final-v2 构建 provenance，不改写历史清单；该测试输入变更后的完整门禁另行执行一次，wrapper 退出 0：fmt/check/clippy 成功，七个非空目标分别 43/27/7/22/21/29/27 项，共 176 项全通过，见 `data/011/final-quality.log`。未重跑或改写既有 release/Collector/矩阵结果。

早期日志/指标复审发现三个 Medium：TLS 握手漏计 tlsfail、关闭时队列排空、最早退出 guard；修复后关闭。core 正式 probe 的 `artifacts/core-probe/report.json` 记录 6000 请求、3000 次 200 和 3000 次 502，阻塞 stderr 未阻塞请求，丢弃 4671 条；16 个 metrics 连接、17th 等待、读头 408。慢写只收到 183777 bytes 中 14297 bytes，约 2.8 秒 EOF，不是完整 200 scrape 证明。140 次成功 reload、130 route、2162 series、失败候选不占槽，FD/线程采样收敛，正常退出 0 且 Socket 消失；该 probe 未覆盖 tracing 或 WS。

官方 Collector H2 测试实际两次 200 正文及同连接 421，三个 server root 对应状态 200、421、200，未含原始 Host/query。首次 KDL 漏分隔符失败保存在 `artifacts/011-h2-collector-kdl-separator-error.log`，补分隔符后重跑退出 0。timeout fixture 首次 Drop 强杀导致尾部 completed span 丢失：native 通过但 Collector 期待两个 root 实际仅一个，见 `artifacts/011-timeout-collector-force-kill.log`。改为优雅 TERM，未放宽 exact-two 断言；最终质量门禁验证旧 generation 1 timeout span 无 status、body known false，新 generation 2 completed 200、body known 2。实际 fixture HTTP 504/200，取消的 span 不能伪填 504。

初次 debug AB 的 generic OSError 无位置，保留 `artifacts/011-trace-ab-setup-oserror.log`；TIME_WAIT 下 bind 的 REUSEADDR 微测试与 openssl 配置诊断改善了后续执行，但不宣称原错误根因确定。早期 core wrapper 中断 self-report 不作正式通过；bind 权限错误保留并随后修复。watch 009 一次 NotFound 未确证根因，专项与后续门禁变绿不代表已根治。

## Release 与互操作

最终 release `artifacts/rill` 为 21,796,728 bytes，SHA-256 `c27d76720c61ea99018a5483b6b409eed1b9b47c2e48eb45e6a449801c078b70`。release smoke wrapper 退出 0：静态 gzip/br、range、HEAD、错误、列表、缓存替换删除及新 observability，实际 HTML 198 bytes，恰好两个 access/request；metrics 不自计，marker 不存在，trace ID null，generation 1，正常退出且 Socket 消失。`artifacts/release-smoke/manifest.json` 的五个 leaf 已核验。

三个 release probe wrapper 均退出 0，driver result 与 manifest hash 已核：

- A：四环境变量拒启，未见 marker/listener、Socket 不存在；观察为采样而非覆盖全部时序。B1：off 在 0.3 秒观察期无 span；Reload on 后三个本地 root 的 ID 关联、隐私及 SDK 环境覆盖。
- B2：TLS WS/SSE 跨策略 off、拒绝 endpoint 和 20 次 reload，旧 generation 1 两个 session 结束时各有对应 access/span，最终 generation 22；WS 101 completed、框架正文 172，SSE 200 error、正文 27，时长均超过 3 秒并关联。FD/线程离散采样 baseline 25/10、最大 27/10、之后 22/10，不是任意时刻上界。origin headers trust 由另行 Rust 测试覆盖，不是此 Python capture。
- C1：真实 HTTP 错误 peer 而非 Collector 解码，超限 Content-Length、超限 chunked、redirect、stall 四次业务均 200；固定 POST `/v1/traces` protobuf、无凭据，export failure 0 至 4，redirect 接收 0。
- C2：并发 16、6000 次全 200 无错误，同一 Rill PID 在故障端点切换到真实 Collector 后恢复，root/access ID 关联且记录 20 个 proxy 访问；stall 时 trace drop 3952、log drop 199，export failure 从 0 至 17。FD/线程 warm 20/22、恢复 21/22 为采样点；2048 队列许可由代码和核心单测证明，非此 probe 直接测量。

证据分别在 `artifacts/trace-release-ab/`、`trace-release-c1/`、`trace-release-c2/`：49 文件 54166 bytes、16 文件 89006 bytes、13 文件 2161646 bytes。静态复审 NoIssues 不是实测替代品。

## 测量方法与阶段状态

仓库 `scripts/bench/observability_ab.py` 是薄入口，runner 复用 traffic、metrics 和真实 backend，另用官方 Collector receiver。同一 release，off/access/metrics/trace/all 五模式各三轮、每轮预热 2 秒测量 30 秒，C8、1 KiB proxy；server 逻辑 CPU 4,5、client 6,7、backend 8,9、Collector 10,11，affinity 不重叠但不是 isolcpus 或 cgroup 隔离。client 资源仅 pid-self，不计子进程。release SHA 如上，锁文件 SHA `8e3b94586d66ead70f8fc82ee38d15b9ef9d0d2e08c0b6abb82e0f6ae48eecb6`，source-final-v2 manifest SHA `bb93a9e81b06307362673a5aa12a0e55456daae058c58ed1d9ad556bdbdcafd3`。runner 独立核二进制 hash；SDK 版本由父流程从 lock 提供，不是从二进制抽取。provenance 必须含 Collector version 和 README 指定五个 SDK 包名。

矩阵 scratch 8 GiB，driver AS 软 2/硬 8 GiB，server/backend 各 2 GiB，Go 8 GiB、GOMEMLIMIT 128 MiB，文件 512 MiB、CPU 900 秒、nproc 128、nofile 512、wall 900 秒。测量修订明确记录实际 rlimits，011 固定关闭重试；真实 mini 测试第二请求 header EOF，旧默认重试得到 2208 样本、2209 物理请求、零错误、valid true，严格模式 2297 样本、2297 物理请求、1 EmptyResponse、raw 1、valid false。不是 mock；保留交通开关兼容旧默认。端口预检考虑 TIME_WAIT/REUSEADDR；固定案例不自动重试或过滤坏行。

pilot 为五模式各一轮 2 秒，实际 0/5 valid true；off/access/trace 有 client warning。只有 metrics/all 有 counter delta，all 的 log drop 2851、trace drop 28、export failure 0；禁用指标为 null，不为测量启用端点。独立标准库 raw verifier 经源码复审修订；父流程在只读输入、无外网、scratch 32 MiB、AS 1 GiB、CPU 300 秒、nproc 16、nofile 64、文件 4 MiB、wall 360 秒沙箱执行 pilot 核验，退出 0，107645 行、47 文件、27757495 bytes，报告 `artifacts/independent-raw-check-pilot.json`，SHA-256 `9d1989ba15c7ee87e78294f569d56523422990bb5e091d157cd80671ca55d3c2`。

### 正式矩阵与独立 raw 核验

正式矩阵执行一次，wrapper 退出 0，15/15 轮有效；所有子进程正常退出、未强杀、无残留进程组，端口和控制 Socket 清理通过。独立标准库核验器在上述只读输入沙箱中实际退出 0：逐行检查 7,306,995 条请求，135 个文件共 1,100,820,560 bytes，文件集合、大小、SHA、严格起始窗口、状态/正文、一次物理请求、计数、最近秩分位数、排空和资源采样峰值均与 summary 一致。原始 CSV 按完成时写入，不要求 begin 全局单调。

证据：[正式摘要](data/011/full-summary.json)、[原始清单](data/011/full-manifest.json)、[独立核验](data/011/independent-raw-check-full.json)。独立报告 SHA-256 为 `001f165cf9d740eaae0abc6a8611edcbf15fabc517548ffce8dd1627d2606cad`。二进制和 SDK 版本的来源仍是上述父流程 provenance，不冒充从原始 CSV 或二进制反向证明构建包；重连、预热边界、lag 原始观测和底层 CPU ticks 没有足够 raw 独立重构，报告明确列为不可独核。

下表 RPS 为三重复中位数及最小–最大范围，P50/P99 为三轮各分位数的中位数；CPU 为三轮 server 单秒采样最大值，RSS 为三轮 server 采样最大值，均非全时序上界。

| 模式 | 窗口完成 RPS，中位数（范围） | P50 ms | P99 ms | server CPU 最大 % | server RSS 最大 MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| off | 16693.23（14276.63–16778.30） | 0.432 | 0.882 | 153.81 | 18.35 |
| access | 16531.37（16507.20–16716.50） | 0.434 | 0.869 | 173.78 | 18.22 |
| metrics | 16888.43（16573.50–17166.20） | 0.428 | 0.859 | 154.81 | 18.00 |
| trace | 16128.33（15940.33–16296.33） | 0.434 | 0.925 | 162.75 | 18.62 |
| all | 15601.07（15533.93–15931.27） | 0.443 | 0.960 | 175.77 | 18.74 |

15/15 轮均触发 client warning，客户端采样 CPU 最高 97.86%；后端最高 96.89%、Collector 最高 8.99%，后两者不计入 server CPU。相对 off 的 RPS 中位数，access 观察为 -0.97%、metrics +1.17%、trace -3.38%、all -6.54%；all 的 P99 中位数为 +0.078 ms。首轮 off 较低的 14276.63 RPS 原样保留，不删除异常轮、补测或据此宣称指标带来收益。该闭环、客户端受限矩阵不能给出容量、精确服务端开销比例或统计显著性结论。

只有 metrics/all 的诊断快照可用。metrics 三轮四项 delta 均为 0；all 三轮 log queue drop 为 11140/12429/9831，trace queue drop 为 226/262/248，两项 export failure 均为 0。access/trace/off 没有 metrics 端点，其计数是 null，不能据此说无丢弃。丢弃说明有界 best-effort 观测不能保证记录完整，不能把正常业务响应等同所有日志/span 成功送达；快照不是精确窗口，也不包含关闭后全部尾部结果。默认访问日志、指标监听和追踪继续关闭，不把本次测量解释为应默认开启。

## 实际执行与验收映射

外层统一使用 `unshare --mount --net --propagation private`，再由各固定 wrapper 执行上述 bwrap/setpriv/prlimit 约束；阶段目录为 `/var/tmp/rill-011.nfYINU/`。关键实际命令：

```sh
bash run-static-red-green-quality.sh source-final-v2 source-static-red
bash run-quality.sh source-final-v3
bash run-release.sh source-final-v2
bash run-release-probe.sh trace-release-ab trace-ab
bash run-release-probe.sh trace-release-c1 trace-c1
bash run-release-probe.sh trace-release-c2 trace-c2
bash run-observability-matrix.sh pilot
bash run-raw-check.sh pilot
bash run-observability-matrix.sh full
bash run-raw-check.sh full
```

以上最终 wrapper 均退出 0；红阶段内部预期退出 101 后才执行绿阶段。质量门禁实际包括 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings` 和 `cargo test --locked --offline --no-fail-fast -q -- --include-ignored --test-threads=2 --nocapture`；发布构建为 `cargo build --release --locked --offline -q`。

| 任务及验收 | 实际证据 |
| --- | --- |
| 011-T01 / AC01 | `core-report.json`、`trace-ab.json`、`example-result.json`：普通/失败、堵 stderr、TLS WS/SSE 完成、字段与 ID/隐私检查 |
| 011-T02 / AC02 | `core-report.json`、`quality.log`：16 连接、慢读写、失败候选、140 Reload、2162 series、请求/上游/H2/传输回归 |
| 011-T03 / AC03 | `h2-result.json`、`timeout-result.json`、`trace-ab.json`、`trace-c1.json`、`trace-c2.json`：官方 Collector、信任传播、故障隔离及同 PID 恢复 |
| 011-T04 / AC04 | `full-summary.json`、`independent-raw-check-full.json`、配置/架构及 benchmark 运维说明、示例 smoke |

所有表中 JSON/log 均位于 [本阶段数据目录](data/011/README.md)。最终实现只读复审 `1c4054b3` 为 NoIssues/MergeOK，测量驱动两项 Medium 在 `bc31992f` 关闭，文档两项 Medium 在 `9638d8ca` 关闭；均不代替动态验证。最终交付一致性复审 `c598aa82` 为 NoIssues/MergeOK：176 项最终日志及 44 个证据条目的仓库/外部字节与 SHA 均一致，总计划过时状态已修复；该只读复审不重新执行原始数据核验或 Collector。

## 解释限制

客户端单核 CPU ≥90% 或 lag ≥100 ms 警告不等于容量、性能收益或显著性。RSS 求和可能重复共享页，HWM 是历史值，没有 PSS/cgroup；不能当作 span queue、缓存正文预算或 RSS cap。指标前后快照包含 readiness、预热、cohort 和异步回调，不是精确窗口，shutdown 前 after 也可能漏尾部；disabled 是 null，不造零。export failure 计失败批次而非丢失 span 数，log/trace drops 应原样保留。

记录按完成写入，begin 不全局单调；每行须满足 measure ≤ begin < deadline。成功 issue cohort 使用最近秩分位数，区分 nominal 窗口完成速率与含排空 cohort 速率。重连、预热边界和在线 lag 不能仅从 raw 独立重建；raw 也不证明全部时序、FD 无泄漏或任意代次长期持有。不同环境的 008、010 和用户 three-way 不直接相减归因。

## 提交状态

实现、176 项最终门禁、release/示例、互操作/故障检查、正式矩阵独立核验及最终实现/交付复审均通过，011 已验收。本次独立完成提交由父流程执行并核对，标识见 Git 历史；不 push，不改写历史。012 未开始。
