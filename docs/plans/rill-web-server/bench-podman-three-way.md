# 三目标 Podman 性能对比计划

## 元数据

- 计划类型：008-T02 性能基线的扩展执行，不开启 009–012
- 实施状态：B1–B7 已完成；结果见 [三目标 Podman 性能对比](../../validation/bench-three-way.md)
- 对照目标：Rill、Nginx、Caddy
- 运行形态：三者均以 Podman 容器运行，同一宿主、同一矩阵、同一限制
- 决策来源：用户访谈（2026-11-03），见本文"已确认决策"

## 目标与范围

在现有 `scripts/bench/` 上扩展出第三个对照目标 Caddy，并把三个目标全部改为容器化运行，产出一份同环境、同矩阵、同限制的三方实测对比及验证记录。

包含：

- HTTP/HTTPS × 代理/1 KiB 静态/16 MiB 静态 × cold/warm × 三轮
- WS/SSE 10/100 连接
- 空闲资源
- Reload（含 Caddy）
- 逐轮资源采样、环境快照、原始数据保留与独立复核

不包含：

- 容量饱和结论（闭环负载给不出容量上限）
- HTTP/2、HTTP/3、QUIC
- 冷磁盘/清空页缓存
- rootless Podman
- 三方之外的实现（Pingap、Envoy 等）
- 009–012 的任何功能实施

## 已确认决策

| 编号 | 决策 | 影响 |
| --- | --- | --- |
| B-D01 | 扩展现有 `scripts/bench/`，不新建并行脚本 | 保留已验收的正确性、分位数、资源、Reload 口径 |
| B-D02 | Nginx 使用 `docker.io/library/nginx:stable-alpine`（本地缓存，1.30.5） | 与 008 的 1.28.3 不可直接横向比较，需新基线 |
| B-D03 | 每次运行创建专用 bridge 网络，后端也是容器；负载生成器在宿主经 `127.0.0.1` 发布端口访问 | DNAT 开销对三方相同；后端经网络别名 `backend:19090` 提供上游 |
| B-D04 | 沿用 Python asyncio 客户端作为唯一负载口径 | 与 008 可比；warm 场景会继续触发客户端限制警告，必须如实披露 |
| B-D05 | 跑 008 标准全量矩阵 | 每目标 54 轮，三方共 162 轮，纯负载时长约 2 小时，含启动与校验约 2.5–3 小时 |
| B-D06 | Reload 场景包含 Caddy，以新连接观察到 `max-age` 变化为成功判据 | 三方对等；Caddy 需额外验证 admin API 与旧连接行为 |
| B-D07 | 三方资源限制完全一致：`--cpuset-cpus` 两核、`--memory 512m`、`--pids-limit 64`、`nofile 4096`、根只读、`--cap-drop=ALL`、`no-new-privileges` | 与 `deploy/podman/README.md` 预检 profile 一致 |
| B-D08 | 先跑 B6 pilot 观察宿主噪声，再决定是否清场 | 阈值命中时可直接暂停宿主负载并重跑 pilot |
| B-D11 | 噪声决策门阈值：被测角色 CPU 集之外的宿主 CPU 平均不超过总容量 5%、`nr_throttled` 为零、同场景三轮 p50 相对离散不超过 10% | 任一不满足即判定当前环境不足以作为基线 |
| B-D12 | 已授权：B6 pilot 命中噪声阈值时，暂停或停止宿主既有负载后重跑 pilot | 暂停前记录原进程与启动方式，pilot 通过后恢复 |
| B-D13 | Reload 场景中 WS/SSE 断连单列报告，不判该轮无效 | 与 nginx 退休旧 worker keepalive 的既有处理一致 |
| B-D09 | 已授权：拉取固定 digest 的官方 Caddy 镜像、`cargo build --release --locked`、`podman build` 构建 `localhost/rill:bench` | 不再就这三项重复请求授权 |
| B-D10 | B6 之外的时段不主动停止宿主既有负载 | 全量运行期间若再次出现噪声，按 B-D13 记录并停下询问 |

## 当前设备事实

- Podman 5.4.2，rootful（非 rootless），cgroup v2 + systemd 管理，netavark 后端，默认 bridge 网段 `10.88.0.0/16`，seccomp 开启，SELinux/AppArmor 关闭。
- 12 逻辑 CPU = 6 物理核，SMT 兄弟对为 `(0,6) (1,7) (2,8) (3,9) (4,10) (5,11)`。
- 内存 31 GiB，内核 7.2.11-x64v3-xanmod1，x86_64。
- 角色分片沿用 008：server `0,1`、client `2,3`、backend `4,5`。每个角色内部的两颗 CPU 属于不同物理核；`6–11` 为主核兄弟与宿主噪声区，需在报告中标注 SMT 干扰可能。
- 镜像现状：`nginx:stable-alpine`（1.30.5，Alpine 3.24.2，digest `sha256:8f84ed99befc3891b8f329c5c202785278a2cfb7c25107d57fb2a134a3117433`）、`nginx:1.28-alpine`、`debian:trixie-slim` 已缓存；Caddy 未缓存，当前上游最新为 v2.11.7。
- `target/release/rill` 不存在，需 `cargo build --release --locked`（rustc 1.99.1，满足 `rust-version = 1.95`）。
- 宿主当前不干净：`hindsight-api` 约 119% CPU、podman 约 46%、多个 node 线程 15–17%、qbittorrent 8.4%，loadavg 1.40。

## 公平性契约

以下条件对三个目标完全一致，任何一项不一致即该轮无效，不得只对部分目标放宽：

| 维度 | 取值 |
| --- | --- |
| 镜像 | 各自官方镜像，固定 tag + digest，记录 image ID |
| 镜像内负载 | 均为非 root UID/GID 10001，根文件系统只读 |
| 可写面 | 仅 bind mount 的 `/work`；必要时 `--tmpfs /tmp:rw,noexec,nosuid,size=16m` |
| capability | `--cap-drop=ALL --security-opt=no-new-privileges` |
| CPU | `--cpuset-cpus 0,1`，不设 `--cpus` 配额（避免配额节流与 cgroup 计费混淆） |
| 内存 | `--memory=512m --memory-swap=512m` |
| 进程/句柄 | `--pids-limit 64 --ulimit nofile=4096:4096` |
| 网络 | 同一 bridge 网络，服务端口发布到 `127.0.0.1:18080/18443`，后端 `backend:19090` 不发布 |
| 停止 | `--stop-timeout 35` |
| TLS | 同一套 fixture 证书，TLS 1.2/1.3，关闭 session cache 与 tickets |
| 静态内容 | 同一 `www` 目录只读挂载，同一 `Cache-Control: max-age=10/20` |
| 访问日志 | 三者均关闭 |
| 压缩 | 三者均不启用 |
| 上游 | 同一 Python 后端，上游 keepalive 开启，代理不缓冲 |

Nginx 需显式写入并在报告中保留：`worker_processes 2`、`sendfile on`、`open_file_cache off`、`keepalive 32`、`proxy_buffering off`、`ssl_session_cache off`、`ssl_session_tickets off`。

## 目标驱动契约

新增 `scripts/bench/targets.py`，把两目标分支收敛为数据驱动的目标描述（每组字段都要有承担者）：

| 字段 | Rill | Nginx | Caddy |
| --- | --- | --- | --- |
| 镜像 | `localhost/rill:bench`（debian trixie-slim + release 二进制） | `nginx:stable-alpine` | 官方 caddy 固定 digest |
| entrypoint | `rill run --config /work/rill-active.kdl` | `/usr/sbin/nginx -e stderr -p /work/ -c /work/nginx-active.conf -g 'daemon off;'` | `caddy run --config /work/caddy-active.json --adapter json` |
| 配置后缀 | `kdl` | `conf` | `json` |
| 配置检查 | `rill check --config` | `nginx -t` | `caddy validate --config` |
| Reload | `podman exec <c> rill reload --socket /run/rill/control.sock --timeout 35s` | `podman exec <c> nginx -t` 后 `podman kill -s HUP <c>` | `podman exec <c> caddy reload --config /work/caddy-active.json --adapter json` |
| 版本命令 | `rill --version` | `nginx -v` | `caddy version` |
| 额外可写路径 | `/run/rill`（0700，UID 10001） | `/work` 内五个 `*_temp` 目录、`/work/nginx.pid` | `XDG_DATA_HOME`、`XDG_CONFIG_HOME` 指向 `/work/caddy` |

Rill 管理 Socket 目录须由宿主 UID 10001 拥有且 0700，与 `deploy/podman/README.md` 的 rootful 要求一致；Socket 为 0600。

## 实施阶段

### B1：环境与镜像准备

- 交付：release 二进制、`localhost/rill:bench` 镜像、固定 digest 的 Caddy 镜像、基础镜像 digest 记录。
- 行动：`cargo build --release --locked`；记录二进制 SHA-256、`ELF` interpreter 与 NEEDED；`podman build --network=none --pull=never --build-arg BASE_IMAGE=...@sha256:...` 构建 Rill 镜像；拉取 Caddy 固定 digest 并记录 `podman image inspect`；离线确认三个镜像的版本命令可执行。
- 验收：三个镜像可 `podman run --rm` 输出版本；Rill 镜像在 `--read-only --user 10001:10001 --cap-drop=ALL` 下能 `check` 合法配置。
- 阻塞条件：Caddy 无法关闭 TLS session tickets 或无法以 UID 10001 + 只读根运行，则停下询问，不以放宽限制绕过。

### B2：容器运行时与采样改造

- 交付：`scripts/bench/container.py`（创建/启动/停止/exec/宿主 PID 解析/cgroup 路径解析/残留检查）、`metrics.py` 的容器感知采样。
- 行动：采样根不再使用 `Popen.pid`，改为 `podman inspect -f '{{.State.Pid}}'` 得到的宿主侧容器 PID，再按 PPID 递归；liveness 用 `podman inspect -f '{{.State.Running}}'`；停止用 `podman stop --time 35` 并核对退出码、容器已删除、无残留组。新增每轮 `-cgroup.csv`，记录 `cpu.stat usage_usec` 增量、`memory.current`、`memory.peak`、`cpu.stat nr_throttled`，与现有 `-resources.csv` 并存，不改动既有列。
- 验收：单目标短跑证明宿主 PID 解析正确（容器内 PID 1 与宿主 PID 一一对应）、`cpu_percent` 非零且量级合理、`-cgroup.csv` 与 `-resources.csv` 的服务 CPU 差异可解释。
- 说明：现有 `-resources.csv` 列结构不变，历史数据仍可解析。

### B3：fixtures 扩展

- 交付：`fixtures.py` 生成 Caddy 原生 JSON；后端主机可参数化。
- 行动：Caddyfile 模板生成后经容器内 `caddy adapt` 转 JSON，再注入 `apps.tls.session_tickets.disabled = true`，写入 `caddy-a.json`/`caddy-b.json` 与 active 副本；关闭 admin 端口发布、限制 `protocols h1`、`auto_https off`、`grace_period 30s`；静态分支 `Cache-Control` 值必须严格等于 `max-age=10`/`max-age=20`（不带 `public,` 前缀），否则既有 `get_small` 判据失败；代理分支覆盖 `/proxy`、`/ws`、`/events` 至 `http://backend:19090`。
- 验收：`caddy validate` 通过；三方配置在 `check`/`-t` 阶段均成功；同一请求在三方返回相同状态、长度与字节内容。

### B4：bench.py 三目标化

- 交付：`--targets rill,nginx,caddy`、目标循环、环境元数据、完成度判定。
- 行动：去掉 `('rill','nginx')` 硬编码；`environment.json` 改为 `targets` 映射（image、image_id、digest、version、binary_sha256、config_sha256、限制、网络、启动命令）；`complete` 判定改为"全部目标 × 全部所选组"；保留 `partial`/`invalid` 语义；标签与文件名沿用 `name-scenario-repeat` 形式。
- 验收：`--targets rill --only idle --repeats 1` 可独立完成并产出 `complete`；`--targets` 非法值退出码为 2。

### B5：Reload 三目标化

- 交付：`reloading.py` 分支收敛到目标描述，Caddy 走 admin API。
- 行动：Caddy reload 后以新 cold 连接在 5 秒内观察到目标 `max-age` 为成功判据，`/load` 返回 200 不算成功；记录调用耗时、返回码、实际生效观测点；旧连接行为与 WS/SSE 断连单独记录，不与逻辑错误混算。
- 验收：单目标 `--only reload --repeats 1 --reload-seconds 8 --reload-interval 2 --reload-count 3` 三方各自通过；故意写入非法配置时旧配置继续生效且判定为失败。

### B6：噪声 pilot 与决策门

- 交付：pilot 结果与噪声评估。
- 行动：用三方各一轮 `--only http --repeats 1 --http-seconds 10` 加一次 30 秒 idle 采样；记录每轮被固定 CPU 集之外的宿主 CPU 占比、loadavg、逐核占用、`nr_throttled`。
- 判定门：若被测角色 CPU 集之外的宿主 CPU 平均超过总容量 5%，或 `nr_throttled` 非零，或三轮同场景 p50 相对离散超过 10%，则判定当前环境不足以作为基线。命中后按 B-D12 暂停宿主既有负载、重跑 pilot；暂停前记录进程与启动命令，pilot 通过后恢复，恢复失败立即报告。
- 明确不在授权内：卸载软件、修改 systemd 单元、改动宿主 sysctl 或 governor。
- 验收：形成"可继续"或"已清场后通过"或"清场后仍不通过"的明确结论并记录证据；第三种情况停下询问。

### B7：全量运行、复核与文档

- 交付：原始产物目录、`docs/validation/008-performance-podman-three-way.md`、`scripts/bench/README.md` 更新、`README.md` 文档索引一行。
- 行动：按 B-D05 矩阵运行 162 轮；每轮保持 `--targets` 集合与重复次数一致；原始逐请求/逐事件 CSV 独立保留，Git 只提交摘要、资源记录、环境与校验清单（沿用 008 的取舍）；随后用独立于生成的核对脚本复查分位数可复算、轮次数与标签齐全、资源与 cgroup 记录对齐。
- 验收：162 轮全部 `valid` 且进程/容器正常退出无残留；`environment.json` 为 `complete`；摘要表与原始数据一致；报告明确列出闭环负载、客户端限制、SMT 与宿主噪声、Nginx 版本变更导致与 008 不可直接比较。

## 结果口径

- 沿用 008：最近秩分位数、issue cohort、`completed_rps_nominal` 与含排空的 cohort 速率分开报告、逻辑错误率与物理 attempts/重试分开统计。
- cold 指新建 TCP/TLS 连接，不清页缓存。
- 资源：`-resources.csv` 为进程树求和（RSS 可能重复计共享页），`-cgroup.csv` 为 cgroup 计费口径，两者并列，不互相替代；不测 PSS。
- 服务 CPU 只统计容器内服务，不含宿主机 DNAT/softirq 与负载生成器开销，报告中单列宿主侧总量。
- 客户端单核 ≥90% 或事件循环最大滞后 ≥100 ms 触发警告；警告不等于饱和，无警告也不证明未饱和。
- 表内数字为三轮单轮指标的中位数，不合并样本。

## 预计时长

- 每目标 54 轮：HTTP 36 轮 ×（2 s 预热 + 30 s 测量）、streams 12 轮 × 60 s、idle 3 轮 × 60 s、reload 3 轮 ×（2 s + 60 s）。
- 单目标纯负载约 39 分钟，三方约 1 小时 57 分钟；含镜像准备、每轮校验与容器启停约 2.5–3 小时。产物量级按 008 的 371 MB/108 轮线性外推，约 550 MB 原始数据，需预留磁盘。

## 风险

| 风险 | 严重度 | 缓解 |
| --- | --- | --- |
| 容器内 `Popen.pid` 不是服务进程，资源数据失真 | 高 | B2 显式以 `podman inspect` 宿主 PID 为采样根，并在 B2 验收中交叉验证 cgroup 口径 |
| 后端经 bridge 别名访问，与 008 的 loopback 路径不同 | 中 | 三方路径完全相同；报告中声明与 008 路径差异，不与 008 数字直接相减 |
| Nginx 版本从 1.28.3 变为 1.30.5 | 中 | 报告中明确新基线，不沿用 008 对比结论 |
| Python 客户端在 warm 场景成为瓶颈 | 高 | 保留并如实披露警告；不把三方差异解释为容量上限 |
| 宿主噪声（119% CPU 单进程等） | 高 | B6 决策门；清场需授权 |
| SMT 兄弟与 6–11 号 CPU 上的噪声互相干扰 | 中 | 记录 `cpu.stat nr_throttled` 与逐核占用；必要时改选物理核 |
| Caddy 无法等价关闭 TLS 会话复用 | 中 | B1 阻塞条件；不允许对部分目标放宽 TLS 限制 |
| Caddy 旧配置下 WS 在 reload 时断开 | 低 | 单独记录断连，不与逻辑错误混算 |
| 512 MiB 内存上限先于 CPU 成为瓶颈 | 低 | 记录 `memory.peak` 与 OOM 计数，超限即判定该轮无效 |
| 运行时长与磁盘占用超预期 | 低 | B6 pilot 后校准；必要时分批执行并保留中间产物 |

## 验证与测试边界

不新增单元测试或测试框架。验证方式为：B2 的单目标短跑交叉核验、B5 的缩短 Reload 试运行、B6 的噪声 pilot、B7 的全量运行与独立数据复核。若实现中出现可复现的采样或解析缺陷，只补一条最小回归。

## 执行结果

B1–B7 全部按计划完成，162 轮全部有效，容器与网络无残留，`environment.json` 为 `complete`。噪声判定门三项均通过（角色集外占用 3.6%、`nr_throttled` 全为 0、pilot 重跑后 p50 离散最大 5.7%），因此未清场宿主负载；采样方式、算法与重跑数值见 `docs/validation/data/bench-three-way-evidence/noise-gate.txt`，首次 pilot 的 108 个摘要归档在同目录 `pilot1/`。实测数据、口径说明与独立核验见 [三目标 Podman 性能对比](../../validation/bench-three-way.md)，原始摘要与资源记录在 `docs/validation/data/bench-three-way/`。

实际执行与计划的差异：

- 计时使用 `/var/tmp`，因为 `/tmp` 是 16 GiB tmpfs 且已接近占满，写满会让运行失败。
- 矩阵按计划为每目标 54 轮、共 162 轮；纯负载与校验合计约 2 小时 40 分钟。
- 实现期间经独立复审修复了容器生命周期、采样完整性、权限与公平性披露等问题，详见完成提交。

## 未决事项

无。B1–B7 的实施授权、轮次规模、噪声阈值、清场处置与 Reload 断连口径均已确认并执行。
