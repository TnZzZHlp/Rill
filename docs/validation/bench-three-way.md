# 三目标 Podman 性能对比实测

## 状态

Rill、Nginx、Caddy 三目标全量标准矩阵于本机完成：162 轮全部有效，容器全部正常退出并删除，网络已清理，无残留。`environment.json` 为 `complete`，`process-exits.json` 的 `residue` 为空。

本记录只描述一次真实执行在同一宿主、同一限制、同一网络路径下得到的结果。闭环负载不构成容量上限，客户端存在限制，宿主存在噪声，跨机或跨版本不可直接沿用。计划与判定口径见 [三目标 Podman 性能对比计划](../plans/rill-web-server/bench-podman-three-way.md)；脚本契约见 [scripts/bench/README.md](../../scripts/bench/README.md)。

## 环境

- 宿主：x86_64，12 逻辑 CPU（6 物理核，SMT 兄弟对 `(0,6)(1,7)(2,8)(3,9)(4,10)(5,11)`），31 GiB 内存，内核实测 7.2.11-x64v3-xanmod1。
- 运行方式：Podman 5.4.2，rootful，cgroup v2（systemd 管理），netavark，seccomp 开启。
- CPU 分片：服务 `0,1`、负载生成器 `2,3`、后端 `4,5`；三组互不重叠，组内两颗逻辑核属于不同物理核（`server_cpus_share_core.smt_shared=false`）。
- 网络：每次运行专用 bridge `172.30.0.0/24`，网关 `172.30.0.1`，后端固定 `172.30.0.2:19090` 且不发布；负载生成器在宿主经 `127.0.0.1:18080`（HTTP）与 `127.0.0.1:18443`（HTTPS）发布端口访问。
- 三方完全相同的容器限制：`--cpuset-cpus 0,1`、`--memory 512m --memory-swap 512m`、`--pids-limit 64`、`nofile 4096`、根文件系统只读、`--cap-drop=ALL`、`--security-opt=no-new-privileges`、`--stop-timeout 35`、服务 UID/GID 10001。

| 目标 | 镜像 | 镜像 ID | 版本 |
| --- | --- | --- | --- |
| Rill | `localhost/rill:bench`（debian trixie-slim + release 二进制） | `2746d9e93ddb` | `rill 0.1.0` |
| Nginx | `docker.io/library/nginx:stable-alpine` | `43d9d8c1f896` | `nginx/1.30.5` |
| Caddy | `localhost/caddy:bench`（官方 caddy 固定 digest） | `f137b0799281` | `v2.11.7` |

以下为构建时的记录，不是本次运行产物的一部分（`environment.json` 只记录镜像 ID 与 digest）：release 二进制 SHA-256 `49f8cebd7ea5af60a061d0e78becef519acfe289eb8e7daaf7027cef71171ec1`，`readelf` 显示 interpreter `/lib64/ld-linux-x86-64.so.2`、NEEDED 为 `libgcc_s.so.1`、`libm.so.6`、`libc.so.6`，即动态链接，不是完全静态二进制。要复现该断言，需重新 `cargo build --release --locked` 并核对哈希。

## 公平性契约

所有目标满足同一契约，任一不满足则脚本拒绝启动：同一静态根与证书只读挂载、同一 `Cache-Control: max-age=10/20`、同一三条上游路径、关闭 access log 与压缩、TLS 1.2/1.3 且关闭会话缓存与 tickets、同一后端与上游 keepalive。

不能完全对齐的差异已写入 `environment.json` 的 `disclosures`，不作为可忽略项：

- 并行度：Rill 的 tokio worker 线程由容器内 `available_parallelism` 决定（cpuset 为 2）；Nginx 显式两 worker 且 `sendfile on`；Caddy 通过 `GOMAXPROCS` 固定为 cpuset 大小（2），否则会按宿主 12 核扩线程。
- 镜像与运行时：Rill 为 glibc + vendored OpenSSL 构建；Nginx 为 musl + 基础镜像 OpenSSL；Caddy 为 musl + Go，带 GC。
- Caddy 的 `session_tickets.disabled` 只存在于 native JSON，由固定镜像内 `caddy adapt` 生成后注入；Caddyfile 单独不支持该选项。

## 结果

各列为三轮单轮指标的中位数，不是合并全部样本的分位数。RPS 为窗口内完成量的名义速率。

### 吞吐与延迟

| 协议 / 内容 / 连接 | Rill RPS | Rill p50/p95/p99 ms | Nginx RPS | Nginx p50/p95/p99 ms | Caddy RPS | Caddy p50/p95/p99 ms |
| --- | ---: | --- | ---: | --- | ---: | --- |
| http / proxy / cold | 3450.5 | 2.121/2.591/3.072 | 3555.3 | 2.072/2.374/2.921 | 3455.6 | 2.116/2.647/3.189 |
| http / proxy / warm | 9941.5 | 0.724/0.937/1.334 | 12888.6 | 0.551/0.755/1.001 | 9693.5 | 0.714/1.130/1.747 |
| http / small / cold | 3579.7 | 2.043/2.453/2.949 | 3618.1 | 2.037/2.346/2.874 | 3608.9 | 2.035/2.410/2.916 |
| http / small / warm | 16811.6 | 0.422/0.574/0.778 | 17009.4 | 0.414/0.552/0.753 | 15437.7 | 0.391/0.895/4.376 |
| http / large / cold | 91.8 | 80.288/141.426/154.482 | 79.9 | 92.794/154.735/160.701 | 96.0 | 75.537/135.171/150.871 |
| http / large / warm | 113.9 | 64.837/124.916/136.247 | 101.9 | 72.134/131.570/144.977 | 122.7 | 60.008/117.207/129.513 |
| https / proxy / cold | 857.1 | 8.581/10.544/15.697 | 781.9 | 9.921/12.511/18.510 | 707.4 | 10.408/14.974/68.455 |
| https / proxy / warm | 9353.8 | 0.754/1.056/1.424 | 10960.2 | 0.637/0.888/1.215 | 9412.9 | 0.709/1.219/1.970 |
| https / small / cold | 871.2 | 8.450/10.355/15.385 | 800.8 | 9.592/12.415/34.441 | 795.4 | 9.246/13.933/25.307 |
| https / small / warm | 10568.1 | 0.678/0.856/1.141 | 11564.6 | 0.601/0.820/1.054 | 11749.2 | 0.599/0.824/1.158 |
| https / large / cold | 45.9 | 162.658/226.206/237.366 | 46.4 | 159.292/228.266/251.177 | 50.6 | 146.888/210.127/233.675 |
| https / large / warm | 50.4 | 147.537/210.703/230.855 | 51.5 | 144.851/209.863/230.349 | 57.5 | 128.820/194.070/208.808 |

### CPU（单逻辑核为 100%，cgroup 与进程树两种口径一致）

| 场景 | Rill | Nginx | Caddy |
| --- | ---: | ---: | ---: |
| http / proxy / cold | 52% | 27% | 70% |
| http / proxy / warm | 98% | 43% | 105% |
| http / small / cold | 51% | 24% | 55% |
| http / small / warm | 151% | 73% | 101% |
| http / large / cold | 83% | 7% | 50% |
| http / large / warm | 107% | 7% | 78% |
| https / proxy / cold | 105% | 102% | 143% |
| https / proxy / warm | 111% | 48% | 108% |
| https / small / cold | 107% | 104% | 154% |
| https / small / warm | 129% | 43% | 100% |
| https / large / cold | 92% | 67% | 65% |
| https / large / warm | 104% | 81% | 75% |

### 内存：进程树 RSS 峰值

| 场景 | Rill | Nginx | Caddy |
| --- | ---: | ---: | ---: |
| http / proxy / cold | 18.0 MiB | 20.1 MiB | 61.9 MiB |
| http / proxy / warm | 18.2 MiB | 20.1 MiB | 64.4 MiB |
| http / small / cold | 18.4 MiB | 20.1 MiB | 63.0 MiB |
| http / small / warm | 18.6 MiB | 20.1 MiB | 63.6 MiB |
| http / large / cold | 20.5 MiB | 20.1 MiB | 62.3 MiB |
| http / large / warm | 20.7 MiB | 20.1 MiB | 62.8 MiB |
| https / proxy / cold | 20.7 MiB | 20.7 MiB | 63.5 MiB |
| https / proxy / warm | 21.0 MiB | 20.7 MiB | 64.9 MiB |
| https / small / cold | 20.8 MiB | 20.7 MiB | 63.6 MiB |
| https / small / warm | 20.6 MiB | 20.5 MiB | 64.4 MiB |
| https / large / cold | 21.0 MiB | 20.3 MiB | 63.1 MiB |
| https / large / warm | 21.5 MiB | 20.4 MiB | 63.7 MiB |

### 内存：cgroup 本轮驻留峰值（每秒 memory.current 的最大值）

| 场景 | Rill | Nginx | Caddy |
| --- | ---: | ---: | ---: |
| http / proxy / cold | 8.0 MiB | 5.0 MiB | 17.6 MiB |
| http / proxy / warm | 8.5 MiB | 5.2 MiB | 22.5 MiB |
| http / small / cold | 9.0 MiB | 4.8 MiB | 19.8 MiB |
| http / small / warm | 9.2 MiB | 4.7 MiB | 21.0 MiB |
| http / large / cold | 115.0 MiB | 102.9 MiB | 104.5 MiB |
| http / large / warm | 56.6 MiB | 117.3 MiB | 84.4 MiB |
| https / proxy / cold | 27.1 MiB | 5.4 MiB | 21.2 MiB |
| https / proxy / warm | 27.6 MiB | 5.7 MiB | 22.5 MiB |
| https / small / cold | 27.3 MiB | 5.3 MiB | 21.1 MiB |
| https / small / warm | 27.2 MiB | 5.1 MiB | 21.8 MiB |
| https / large / cold | 67.8 MiB | 47.2 MiB | 62.7 MiB |
| https / large / warm | 92.9 MiB | 43.7 MiB | 71.0 MiB |

### 内存：cgroup 容器历史峰值上界（memory.peak，不可重置）

同一容器连续跑完整个矩阵，该值只增不减，因此它反映"到此为止的最高水位"，不是本轮峰值。

| 场景 | Rill | Nginx | Caddy |
| --- | ---: | ---: | ---: |
| http / proxy / cold | 8.6 MiB | 18.8 MiB | 23.1 MiB |
| http / proxy / warm | 8.9 MiB | 18.8 MiB | 23.8 MiB |
| http / small / cold | 9.5 MiB | 18.8 MiB | 23.9 MiB |
| http / small / warm | 9.5 MiB | 18.8 MiB | 23.9 MiB |
| http / large / cold | 119.7 MiB | 132.0 MiB | 120.4 MiB |
| http / large / warm | 123.9 MiB | 132.0 MiB | 120.6 MiB |
| https / proxy / cold | 123.9 MiB | 132.0 MiB | 120.6 MiB |
| https / proxy / warm | 123.9 MiB | 132.0 MiB | 120.6 MiB |
| https / small / cold | 123.9 MiB | 132.0 MiB | 120.6 MiB |
| https / small / warm | 123.9 MiB | 132.0 MiB | 120.6 MiB |
| https / large / cold | 123.9 MiB | 132.0 MiB | 120.6 MiB |
| https / large / warm | 123.9 MiB | 132.0 MiB | 120.6 MiB |

### CPU 效率：每 1000 个成功请求消耗的服务 CPU 秒

由 cgroup CPU 峰值与名义速率推算，仅用于同条件横向比较，不是实测能量。

| 场景 | Rill | Nginx | Caddy |
| --- | ---: | ---: | ---: |
| http / proxy / warm | 0.0981 s | 0.0333 s | 0.1086 s |
| http / small / warm | 0.0895 s | 0.0427 s | 0.0655 s |
| https / proxy / warm | 0.1187 s | 0.0442 s | 0.1146 s |
| https / small / warm | 0.1217 s | 0.0371 s | 0.0851 s |

### WebSocket 与 SSE

| 场景 | Rill p50 / p99 | Nginx p50 / p99 | Caddy p50 / p99 |
| --- | --- | --- | --- |
| ws 10 连接 | 0.874 / 1.468 ms (断连 0) | 0.777 / 1.353 ms (断连 0) | 0.812 / 1.287 ms (断连 0) |
| ws 100 连接 | 1.518 / 6.689 ms (断连 0) | 1.521 / 7.104 ms (断连 0) | 1.510 / 6.625 ms (断连 0) |
| sse 10 连接 | 1001.921 / 1058.508 ms (断连 0) | 1001.855 / 1049.718 ms (断连 0) | 1001.940 / 1050.423 ms (断连 0) |
| sse 100 连接 | 1001.874 / 1057.580 ms (断连 0) | 1001.716 / 1052.289 ms (断连 0) | 1001.712 / 1061.816 ms (断连 0) |

### 空闲资源（60 秒无请求）

| 目标 | 线程 | fd | 进程树 RSS | cgroup 本轮驻留峰值 | CPU |
| --- | ---: | ---: | ---: | ---: | ---: |
| rill | 4 | 15 | 28.1 MiB | 33.7 MiB | 0.94% |
| nginx | 3 | 29 | 20.2 MiB | 4.1 MiB | 0.00% |
| caddy | 11 | 9 | 65.7 MiB | 23.7 MiB | 0.00% |

### Reload

| 目标 | p50 | p95 | p99 | max | WS 断连 | SSE 断连 | 窗口内 HTTP p50 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| rill | 169.1 ms | 215.4 ms | 215.4 ms | 224.1 ms | 0 | 0 | 0.434 ms |
| nginx | 342.2 ms | 393.7 ms | 393.7 ms | 397.4 ms | 0 | 0 | 0.423 ms |
| caddy | 209.7 ms | 227.3 ms | 227.3 ms | 374.8 ms | 10 | 0 | 0.397 ms |

### 客户端限制

| 目标 | 触发警告轮次 | 客户端 CPU 峰值 |
| --- | ---: | ---: |
| rill | 33/54 | 104% |
| nginx | 33/54 | 101% |
| caddy | 30/54 | 101% |

## 读数要点

只陈述表中可直接读出的观测，不给因果归因；归因需要额外测量，本记录未做。

- `http/proxy/warm`：Nginx 12888.6 RPS，Rill 9941.5，Caddy 9693.5。该场景三轮里客户端 CPU 峰值为 Nginx 90.5/90.3/91.2%、Rill 82.7/78.1/77.9%、Caddy 74.4/73.5/74.4%，Nginx 三轮都触发警告、另外两者未触发。
- `http/proxy/cold` 与 `http/small/cold`：三方 RPS 在 3450–3618 之间，差异小于 5%。
- `http/large/warm`：Caddy 122.7 RPS，Rill 113.9，Nginx 101.9；`https/large/warm` 同样是 Caddy 57.5 最高。大文件场景服务 CPU 在 8–107% 之间分布，并不普遍低于 1 KiB 场景（例如 `http/proxy/cold` 的 Rill 为 52%，而 `http/large/cold` 的 Rill 为 84%）。
- HTTPS 场景按工作负载分别看：1 KiB 代理 cold 为 707–857 RPS，1 KiB 静态 cold 为 795–871 RPS，16 MiB cold 只有 45.9–50.6 RPS。不能把三者合并成一句"HTTPS cold 在 700–900 RPS"。
- CPU 与内存的同条件对比见上四张表与 CPU 效率表。CPU 上 Nginx 在 1 KiB 场景最低（每千请求 0.033–0.044 CPU 秒，Rill 0.090–0.122、Caddy 0.066–0.115），大文件场景三者 CPU 都低但 Nginx 尤其低（HTTP 7%）。内存上 RSS 与 cgroup 两种口径结论不同，必须分开读。
- 空闲内存：cgroup 本轮驻留峰值 Rill 33.7 MiB、Nginx 4.1 MiB、Caddy 23.7 MiB，进程树 RSS 为 28.1 / 20.2 / 65.7 MiB。注意 Rill 的空闲 cgroup 值高于其负载场景（8–28 MiB），原因是空闲阶段紧接在探测与大文件请求之后，页缓存尚未回收；这说明 cgroup 口径含页缓存，不能等同于常驻堆占用。两个口径不能互相替代，RSS 求和还会重复计共享页。
- WS 消息 RTT 三方 p50 在 0.777–1.521 ms，SSE 事件间隔三方 p50 约 1001.7–1001.9 ms。SSE 数值由后端事件产生节奏决定，不能读作代理转发开销。

## 客户端与宿主限制

- 全 54 轮中触发客户端限制警告：Rill 33、Nginx 33、Caddy 30（下表按目标汇总）。
- 按场景看，同一场景三轮里三方并不总是同时触发。例如 `http/proxy/warm` 只有 Nginx 触发（3/3），Rill 与 Caddy 为 0/3；`http/large/warm` 只有 Rill 与 Caddy 触发，Nginx 为 0/3。因此不能用"某目标总轮次警告数"替代具体场景的判读，也不能因某场景未警告就认为客户端不是瓶颈。
- HTTP 场景客户端进程 CPU 峰值为 Rill 104%、Nginx 97%、Caddy 101%；三者都至少在一个场景接近或超过单核。
- 因此表中任何 RPS 都只代表所选并发下的结果，不是饱和容量；没有警告也不证明未饱和。
- 噪声判定门：按计划在测量窗口内采样宿主 CPU，角色集（0–5）之外占用为 3.6%，低于 5% 阈值；全部 162 轮 `nr_throttled` 为 0；重跑 pilot 后各场景三轮 p50 相对离散最大 5.7%，低于 10% 阈值。三项均通过，因此未清场宿主既有负载。前两项的原始采样与算法、以及重跑 pilot 的完整数值记录在 [噪声判定门证据](data/bench-three-way-evidence/noise-gate.txt)，首次 pilot 的 108 个摘要归档在 `data/bench-three-way-evidence/pilot1/`，重跑 pilot 的 36 个摘要归档在 `data/bench-three-way-evidence/gate3/`，两处都可用同目录 `SHA256SUMS` 校验。
- 仍需说明：宿主在运行期间存在 qbittorrent、pi、UploadRecord2TG 等常驻负载，loadavg 在 1–3 之间波动。这是共享宿主，不是独占基准机。
- SMT 干扰：服务、客户端、后端各占两颗逻辑核（`0,1`、`2,3`、`4,5`），组内两颗属于不同物理核，但它们的兄弟核 `6–11` 未被本脚本隔离，宿主其他负载可能落在那里并与被测角色竞争同一条物理流水线。本记录未量化该影响，也未测 host 侧 DNAT/softirq 的总 CPU，只报告了容器内服务、客户端与后端三组。
- 服务 CPU 只统计容器内被测服务，不含宿主机 DNAT/softirq 与负载生成器开销；客户端与后端 CPU 单列在 `-resources.csv` 的 `client`、`backend` 组。

## 内存口径的一处修正

`memory.peak` 是容器生命周期内的单调累计值，且不能通过写入重置（实测写入被内核忽略），因此同一个容器从第一轮跑到最后一轮时，它会一直保留历史最高水位。实测可见：Rill 在 `http/proxy/cold-1` 读到 6 MiB，经过 16 MiB 大文件场景后，其后所有轮次都读到 123.9 MiB；Nginx 在 `http/large/cold-2` 之后固定为 132.0 MiB。

所以本轮驻留峰值必须用每秒采样的 `memory.current` 最大值，即上表"cgroup 本轮驻留峰值"。三张内存表并列给出，便于核对；`memory.peak` 只作为上界参考，不能用于比较单轮峰值，也不能读成"空闲占用"。

脚本已相应修正：`metrics.py` 新增 `memory_current_max_bytes`，README 说明了该口径。

## Reload

| 目标 | p50 | p95 | p99 | max | WS 断连 | SSE 断连 | 窗口内 HTTP p50 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Rill | 169.1 ms | 215.4 ms | 215.4 ms | 224.1 ms | 0 | 0 | 0.434 ms |
| Nginx | 342.2 ms | 393.7 ms | 393.7 ms | 397.4 ms | 0 | 0 | 0.423 ms |
| Caddy | 209.7 ms | 227.3 ms | 227.3 ms | 374.8 ms | 10 | 0 | 0.397 ms |

Reload 耗时定义为"发出重载调用 → 新 cold 连接观察到目标 `max-age`"，因此包含各自的配置检查、加载和生效时间，不是纯信号往返。Nginx 每次先执行 `nginx -t` 再发 SIGHUP，因此其耗时包含一次完整配置检查；这解释了它高出一截的一部分，但本记录未做拆分测量，不能断言它是主要原因，也不能据此比较三者内部机制优劣。

Caddy 每轮 10 个 WS 断连是旧配置下连接被关闭的预期行为，按计划单列报告，不判该轮无效；SSE 未断连。Rill 与 Nginx 该场景 WS/SSE 断连均为 0。

## 独立核验

以下检查由独立于生成流程的脚本执行，不调用 `scripts/bench` 的统计代码：

- 分位数可复算：执行后立即用原始逐请求 CSV 按最近秩重算了 54 个 HTTP 摘要的 p50/p95/p99，0 处不匹配。该重算依赖逐请求 CSV，其体积约 427 MB 且按 008 的既有取舍未入 Git，因此这一项只能由持有原始数据者复查，不能从本仓库数据独立重复；仓库内的摘要、资源与 cgroup 记录仍可用 `SHA256SUMS` 校验完整性。
- 吞吐分母：108 个 HTTP 轮次的名义速率等于 `completed_within_window / measurement_seconds`，cohort 速率等于 `successes / (measurement_seconds + drain_seconds)`，0 处不匹配。
- 资源覆盖：162 个 `-resources.csv` 均含 `server`、`backend`、`client` 三组，162 个 `-cgroup.csv` 均存在且非空，`resource_integrity` 全部为 `ok`（无空服务进程树样本）。
- 完整性：162 个摘要 `valid` 全为 `true`；每目标 54 轮（HTTP 36、streams 12、idle 3、reload 3）；容器退出码全为 0，无强杀，无残留容器与网络。
- 产物：`docs/validation/data/bench-three-way` 保存 498 个摘要、资源、cgroup 与 reload 明细文件，共 2,487,561 字节，附 `SHA256SUMS`。逐请求与逐事件 CSV 共约 427 MB，按 008 的既有取舍不纳入 Git，保留在 `/var/tmp/rill-bench/full/out`。

## 与 008 的关系

008 使用宿主进程、nginx 1.28.3 与 loopback，本记录使用容器、nginx 1.30.5 与 bridge。两者路径、版本和隔离方式都不同，因此本表数字不能与 008 的数字直接相减，也不能把差异归因于单一实现变化。008 的记录保持为历史基线，不被本记录覆盖。

## 未覆盖

- 容量饱和点、固定到达率下的尾延迟、多并发扫描。
- HTTP/2、HTTP/3、冷磁盘与清空页缓存后的表现。
- rootless Podman（脚本对该路径直接拒绝）。
- 长时间稳定性、内存增长趋势与连接泄漏。
