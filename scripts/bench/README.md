# 性能基线脚本

执行结果和适用环境以 [三目标 Podman 性能对比](../../docs/validation/bench-three-way.md) 为准；脚本状态不等于独立验收。`environment.json` 仅在全部所选目标和四组场景都成功、容器正常退出且无残留时标记 `complete`，只跑部分场景为 `partial`，执行失败为 `invalid`。

## 文件职责

- `backend.py`：本地测试后端，在容器内以 `172.30.0.2:19090` 提供 `/proxy`、`/ws`、`/events`。
- `fixtures.py`：三目标配置与测试资源生成、容器内 `caddy adapt`、静态校验。
- `targets.py`：Rill / Nginx / Caddy 的镜像、entrypoint、挂载、检查与 Reload 描述；目标分支只出现在此文件。
- `container.py`：仅此模块调用 Podman：网络、启动、停止、exec、宿主 PID、cgroup 路径、残留检查。
- `traffic.py`：HTTP 流量和逐请求记录。
- `streams.py`：WebSocket 和 SSE 流量。
- `metrics.py`：进程树资源采样与 cgroup v2 采样。
- `reloading.py`：Reload 驱动、生效观测和窗口检查。
- `bench.py`：参数、目标循环、场景编排和结果输出。

## 环境与运行

三个目标都以 Podman 容器运行，任一时刻只有一个被测服务在跑。负载生成器在宿主执行，经 `127.0.0.1:18080`（HTTP）与 `127.0.0.1:18443`（HTTPS）发布端口访问；服务到后端的上游连接走专用 bridge 网络 `172.30.0.0/24`，后端固定 `172.30.0.2:19090` 且不发布端口。

先准备镜像（都不拉取未固定 digest 的内容）：

```sh
cargo build --release --locked
podman build --network=none --pull=never \
  --build-arg BASE_IMAGE=docker.io/library/debian@sha256:REPLACE_WITH_CACHED_DIGEST \
  -t localhost/rill:bench CONTEXT
```

Caddy 镜像基于官方固定 digest 构建，只去掉二进制上的 `cap_net_bind_service` 文件能力，使 `--cap-drop=ALL` 下仍可执行：

```sh
podman build --network=none --pull=never \
  --build-arg BASE_IMAGE=docker.io/library/caddy@sha256:REPLACE_WITH_PINNED_DIGEST \
  -t localhost/caddy:bench CONTEXT
```

`--cert`、`--key`、`--ca` 必填，没有默认值，避免在缺省路径不存在时静默使用错误证书。

运行：

```sh
python3 scripts/bench/bench.py \
  --targets rill,nginx,caddy \
  --workdir /scratch/run --output /scratch/run/results \
  --cert /fixture/cert.pem --key /fixture/key.pem --ca /fixture/ca.pem
```

`--targets` 取 `rill`、`nginx`、`caddy` 的逗号子集，默认全选；`--only` 取 `http,streams,idle,reload` 的子集或 `all`。`--workdir` 与 `--output` 必须尚不存在，路径须为本次运行私有。叶证书须覆盖 `bench.test`；CA 必须是完整严格的 CA（critical CA 与 keyUsage keyCertSign，叶证书带 AKID），不能为不合格 fixture 关闭验证。私钥绝不提交，运行目录不接受含引号或换行的路径。

三目标共用同一套限制：`--cpuset-cpus` 由脚本固定为 `0,1`，`--memory 512m --memory-swap 512m`、`--pids-limit 64`、`nofile 4096`、`--read-only`、`--cap-drop=ALL`、`--security-opt=no-new-privileges`、`--stop-timeout 35`、服务 UID/GID 10001。后端固定在 `4,5`，负载生成器在宿主固定在 `2,3`。脚本要求这三组 CPU 互不重叠，并在 `environment.json` 中记录线程兄弟关系；在 6 核 12 线程机器上，每组内部的两颗逻辑核属于不同物理核，但 `6–11` 号核仍可能承载宿主其他负载。

镜像、镜像 ID、digest、版本、容器 cgroup 路径与启动命令都写进 `environment.json` 的 `targets` 段；`config_check` 段保存每个目标对 A/B 两份配置的语法检查结果；`disclosures` 段记录不能完全对齐的差异：Rill 的 tokio 线程数由容器内 `available_parallelism` 决定、Nginx 显式两 worker 且 `sendfile on`、Caddy 由 `GOMAXPROCS` 决定，以及三者基础镜像的 libc 与 TLS 库不同。

## 场景与规模

默认矩阵与 008 相同，每目标 54 轮：HTTP/HTTPS × `/proxy`、`/small.bin`、`/large.bin` × cold/warm × 3 轮 = 36；WS/SSE × 10/100 连接 × 3 轮 = 12；空闲 3 轮；Reload 3 轮。三目标合计 162 轮，纯负载约 2 小时，含容器启停与校验约 2.5–3 小时。缩短试运行：

```sh
python3 scripts/bench/bench.py --targets caddy --only reload --repeats 1 \
  --reload-seconds 10 --reload-interval 2 --reload-count 3 \
  --workdir /scratch/pilot --output /scratch/pilot/out --cert ... --key ... --ca ...
```

间隔乘次数必须严格小于 Reload 秒数。

## 三目标对等契约

- 静态根、证书、`Cache-Control: max-age=10/20` 与三条上游路径对三者完全一致；`Cache-Control` 值必须严格等于 `max-age=<n>`，不能带其他指令，否则 Reload 的 `get_small` 判据失败。
- Nginx：两 worker、`sendfile on`、`open_file_cache off`、上游 keepalive 32、关闭代理 buffering、关闭 access log、`ssl_session_cache off`、`ssl_session_tickets off`。
- Caddy：`auto_https off`、`protocols h1`、`grace_period 30s`、admin 仅监听容器内 `127.0.0.1:2019` 且不发布；配置由固定镜像内 `caddy adapt` 生成后注入 `session_tickets.disabled`；HTTP 与 HTTPS 站点分开声明，避免 `tls` 指令落到明文监听上；`GOMAXPROCS` 显式设为 cpuset 大小（2），避免它按宿主 12 核自行扩线程而与其他目标不可比。
- Nginx 的 Reload 每次先对刚发布的活动配置执行 `nginx -t`，通过后才发 SIGHUP；检查失败直接记为失败，旧配置继续服务。
- 可写挂载按目标各自声明，全部位于 `/work` 之下：三者共用 `/work/active`（原子替换的活动配置）；Nginx 另有 `/work/nginx-temp`（pid 与五个临时目录）；Caddy 另有 `/work/caddy`（`XDG_CONFIG_HOME`/`XDG_DATA_HOME`）；Rill 另有 `/run/rill`（管理 Socket，宿主目录由 UID 10001 拥有且 0700）。其余路径一律只读，包括静态根、证书与私钥。私钥在宿主为 0600 且属 UID 10001。

## 采样与口径

`-resources.csv` 按宿主可见的容器主进程做进程树求和，覆盖 `server`、`backend` 与 `client`（宿主负载生成进程）三组，列为 `monotonic,group,cpu_percent,rss_bytes,hwm_bytes,threads,fds,processes`；容器运行时辅助进程不进入该树。`-cgroup.csv` 并列记录 cgroup v2 的 `cpu.stat usage_usec` 增量、`memory.current`、`memory.peak`、`nr_throttled`、`throttled_usec`、`pids.current`，两者口径独立，不互相替代。cgroup 路径解析失败或任一必需计数缺失、格式非数字时，该轮直接失败，不会以 0 冒充。`memory.peak` 在容器生命周期内单调累积且不能通过写入重置（实测写入被忽略），因此它只是该容器历史峰值的上界，不是本轮峰值；本轮驻留峰值以每秒采样的 `memory_current` 最大值 `memory_current_max_bytes` 为准，`memory.peak_bytes` 仅作上界参考。RSS 求和可能重复计入共享页；cgroup 计费包含页缓存，与 RSS 不可互换；不测 PSS。

HTTP 沿用 008 的最近秩分位数、issue cohort 与双速率口径：`completed_rps_nominal` 只计窗口内完成量，另报含排空的 cohort 速率。cold 指新建 TCP/TLS 连接，不清页缓存。逻辑请求错误率与物理 attempts、idle-close 重连、header-reset 重试分开统计。

Reload 的 `calls` 记录发送时间、目标 `max-age`、返回码、生效观测点与耗时；成功判据是新 cold 连接在 5 秒内观察到目标值，`caddy reload` 返回 0 或 nginx `/load` 式的一次调用本身不算成功。WS/SSE 断连写入 `stream_disconnects` 单列报告，不判该轮无效；连接未全部建立或 HTTP 窗口为空、有错误才判无效。

客户端单核 CPU ≥90% 或事件循环最大滞后 ≥100 ms 触发警告。闭环负载给不出饱和容量，警告只能说明该轮可能受限，未警告也不证明未饱和。

## 清理

每次运行按 `--workdir` 目录名生成唯一容器名前缀，只清理本次实际创建的容器；网络在创建后立即进入清理责任域，任何阶段失败都会停止已创建容器并删除网络。残留复查区分"不存在"与 Podman 自身失败：无法确认残留时同样判失败。不运行全局 prune，不删除他人容器或网络。

参数与历史口径说明见 [008 性能基线](../../docs/validation/008-performance-baseline.md)；该记录使用的是宿主进程与 nginx 1.28.3，不能与本文档所述容器化矩阵直接相减。

## 静态增强 A/B 驱动

`static_ab.py` 是入口，`static_ab_runner.py` 负责编排，`static_ab_transfer.py` 验证并测量表示传输；复用 `traffic.py` 和 `metrics.py`。后者新增 `pid-self` 采样口径以兼容既有进程树口径：客户端只统计驱动自身，不把服务子进程计入客户端。

必须由外部 OS 沙箱提供只读根、仅 loopback 网络、零 capability、UID 65534 和资源限制；驱动不自动隔离，也不拉取容器。预先提供可执行的同一 010 二进制、独立可写私有目录及已验证 sidecar fixture。选择当前允许 affinity 中互不重叠的两颗服务 CPU 和两颗客户端 CPU，端口 127.0.0.1:18080 须空闲：

```sh
python3 scripts/bench/static_ab.py --rill /input/rill \
  --workdir /scratch/static-work --output /scratch/static-output \
  --sidecars-json /input/sidecars.json \
  --server-cpus 0,1 --client-cpus 2,3
```

两个目录均必须不存在、互不嵌套；默认 3 次重复、每轮 30 秒、预热 2 秒、并发 8。identity 的 small（1 KiB）和 large（16 MiB）分别比较缓存关闭及 16 MiB 预算开启，共 12 轮；transfer 在缓存关闭下分别测 identity、gzip、br，共 9 轮，总计 21 轮。`--only identity` 或 `--only transfer` 只产生 partial 结果；`--seconds`、`--warmup`、`--repeats`、`--concurrency` 可调整，调整后不可冒充默认矩阵。

`--sidecars-json` 必填，UTF-8 JSON 包含 `identity_base64`、`gzip_base64`、`br_base64` 三个标准 base64 字符串。identity 必须恰为 1024 个 ASCII `s`；gzip 可用 Python 标准库生成，br 由发布流程提供，并在运行前用独立 Brotli 解码器验证解码结果与 identity 逐字节一致。例如在已有经过独立解码验证的 `/input/small.br` 后生成 JSON：

```sh
python3 - <<'PY'
import base64, gzip, json
from pathlib import Path
raw = b's' * 1024
values = {'identity': raw, 'gzip': gzip.compress(raw), 'br': Path('/input/small.br').read_bytes()}
Path('/input/sidecars.json').write_text(json.dumps({k + '_base64': base64.b64encode(v).decode() for k, v in values.items()}))
PY
```

驱动验证 identity 和 gzip 解码，br 仅与外部已验证 fixture 字节比较，不自行解码；传入参数不构成验证证明。每轮独立服务进程组，仅清理自己启动的 PGID，正常退出码必须为 0、不得强杀且控制 Socket 不残留，否则该轮无效。输出 `summary.json`、逐请求 `*-requests.csv.gz`、`*-resources.csv` 和记录精确 monotonic measure_start、deadline 的 `*-window.json`；失败为 invalid。资源数据的 RSS 可能重复计算共享页，HWM 为历史峰值，非该轮峰值；无 PSS 和 cgroup 数据。

延迟使用最近秩，区分窗口完成的 nominal 速率与含排空的 issue cohort 速率；预热边界、重连和事件循环 lag 是在线诊断，不能仅由逐请求 CSV 重建。压缩统计是编码后的 payload 字节，不是 TCP/HTTP 总线字节。客户端 CPU 达单核 90% 或事件循环 lag 达 100 ms 警告；闭环 RPS 不是容量。配置预算不等于实际使用量或命中率，不从 HTTP metrics 推断命中，缓存行为另由核心功能测试验证。

指标和 OTLP 追踪已移除，对应的可观测性 A/B 驱动不再提供。历史测量记录保留在 `docs/validation/011-observability.md`；资源采样模块 `metrics.py` 仍供其他基准驱动使用，不是服务端指标端点。
