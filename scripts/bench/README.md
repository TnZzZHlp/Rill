# 性能基线脚本

执行结果和适用环境以 [008 验证记录](../../docs/validation/008-p0-validation-deployment.md)为准；脚本状态不等于独立验收。`environment.json` 仅在四组全部成功且进程正常退出时标记 `complete`，子集为 `partial`，执行失败为 `invalid`。

## 文件职责

- `backend.py`：本地测试后端。
- `fixtures.py`：配置和测试资源。
- `traffic.py`：HTTP 流量和请求记录。
- `streams.py`：WebSocket 和 SSE 流量。
- `metrics.py`：资源采样和统计。
- `reloading.py`：Reload 驱动及窗口检查。
- `bench.py`：场景编排、子进程和结果输出。

## 环境与运行

仅在获准的私有网络、PID 和文件系统命名空间中运行，使用私有可写 scratch、启用 loopback、不授予 capability，并在 `/fixture` 提供可信 CA、叶证书和私钥。叶证书须覆盖 `bench.test`；CA 必须是完整严格的 CA（critical CA 和 keyUsage keyCertSign，叶证书带 AKID），不能为不合格 fixture 关闭验证。私钥绝不提交。预先将缓存的 nginx 二进制和 musl 库挂载至 `/reference`，不拉取镜像。`--workdir` 和 `--output` 均不得已存在，output 可为 workdir 下的新目录；路径须为本次运行私有。选择命名空间内实际可用的六个不同 CPU，例如：

```sh
python3 scripts/bench/bench.py --rill /path/to/rill --workdir /scratch/run --output /scratch/run/results --server-cpus 0,1 --client-cpus 2,3 --backend-cpus 4,5 --cert /fixture/cert.pem --key /fixture/key.pem --ca /fixture/ca.pem
```

两个服务器默认独立重复三轮：HTTP 测量 30 秒、预热 2 秒、并发 8；WS/SSE 分别使用 10 和 100 个连接、持续 60 秒；空闲 60 秒；Reload 测量 60 秒、HTTP 预热 2 秒、每 10 秒切换一次共 5 次，同时有 8 个 warm HTTP 客户端、10 个 WS 和 10 个 SSE 连接。默认 `--only all`；可用逗号分隔的 `http,streams,idle,reload` 选择子集。缩短试运行示例：`--only reload --repeats 1 --reload-seconds 8 --reload-interval 2 --reload-count 3`。间隔乘次数必须严格小于 Reload 秒数。每个目标先执行 HTTP 和单连接 WS/SSE 正确性检查。

驱动仅启动自己的后端和一次一个目标，启动前检查端口并记录子进程退出；端口检查不是原子预留，因此必须使用私有网络命名空间。子进程使用独立进程组，先发 TERM，必要时 35 秒后发 KILL。强杀、非零退出、启动、正确性、采样或 Reload 错误均使运行无效。后端是独立必需文件。驱动收到 SIGTERM/SIGINT 时取消异步任务并清理记录的进程组，包括组长已退出的 worker；强制终止或残留组成员使运行无效，命名空间销毁仅为外部兜底。

## HTTP 和统计

每轮校验状态、精确长度和每个字节，保存压缩逐请求 CSV 和逐轮 JSON。头限制为 64 KiB 和 5 秒，body 读取限制 120 秒；每轮最多 200 万测量样本，超过则失败而不丢样本。成功测量请求的 p50/p95/p99 使用 nearest-rank；原始 CSV 可用于重算。

HTTP 延迟分位数属于测量窗口内发起的请求 cohort，包括窗口后排空完成的请求。标称速率只计 `end <= deadline` 的完成数和字节；另行报告计入排空时间的 issued-cohort 吞吐、迟完成数、排空时长和排除的预热边界完成数，不能混用口径。每个 Reload 请求窗口必须至少有一个成功样本且逻辑错误为零；其精确 nearest-rank 统计使用有界基本类型数组，并设有原始样本及总量上限。

warm 使用 HTTP/1.1 已有连接，测量请求至少 95% 必须真实复用；cold 每次新建 TCP 和 TLS 握手，不得复用连接。cold 指连接而非冷磁盘，文件内容及哈希确定且不清除页缓存。TLS 1.2 以上验证提供的 CA、主机名和 SNI，拒绝会话复用；服务端配置关闭会话缓存和 tickets。结果是 HTTP/1.1 而非 HTTP/2 性能。

两目标的幂等 GET 使用相同策略：写入前发现闲置 warm 连接已 EOF 就先换连接；仅首次尝试使用旧 warm 连接时，请求写入或获取响应头阶段出现空头 EOF 或 reset，最多使用新连接重试一次。部分头 EOF、任何 body 失败及其他错误不重试。分别保留 `header_reset_retries`、`physical_attempts` 和 `idle_close_reconnects`，以及原始行中的最终连接请求序号、复用、尝试和连接数；逻辑错误率不能冒充传输失败率，延迟包括重连和重试。Nginx Reload 合法退休空闲 keepalive，重连不隐藏。

## 长连接和 Reload

WS 使用 masked binary 序号回显，报告握手、RTT、关闭及断连；SSE 校验跨 chunk 递增事件序列，分别报告首事件和事件间隔。每轮先连接全部客户端，再开始共同测量窗口。每个 WS 回显和完整 SSE 事件的进度期限为 3 秒，片段、chunk 或 ping 不重置期限；记录每连接尾部年龄及最大无活动间隔，尾部过期使该轮无效。空闲场景不发应用请求，仅记录资源。

Reload 使用不可变 A/B 模板并原子替换活动文件。Rill CLI 必须返回成功；nginx 须先通过 `-t` 再发 SIGHUP。此后新 cold 连接必须在 5 秒内观察到目标 cache 值，仅发送 SIGHUP 不算成功。每次切换记录耗时、配置哈希、返回码及实际 Rill generation；下一轮必要时通过真实确认的 Reload 恢复 A。汇总保留负载、流、窗口和完整阶段结果；HTTP 窗口按请求开始时间属于切换开始至观察成功后一秒（失败时至调用结束）判定。独立 warm 探针检查 cache 值 10 或 20；HTTP 负载逐 body 校验，但不暴露每个负载响应的 cache 头。Nginx 旧 worker 的 keepalive 可继续使用旧配置，Rill 使用逐请求快照；仅因保留旧 cache 值不判定切换失败。测量阶段后关闭流，不强制设置 worker 关闭超时；Rill 正常关闭的 30 秒行为与 nginx 优雅退休不同。

## 资源和比较边界

资源 CSV 每秒通过 `/proc` 采样，分别统计 nginx master 及其后代、后端和客户端。CPU 百分比以单个逻辑核为基准；已观察到的退出进程 CPU 计数保留，但两次采样间创建并退出的进程会遗漏。RSS 为进程树求和，共享页可能重复；HWM 为各进程高水位之和，不是同时峰值；不测 PSS。

逐轮 JSON 包含观测峰值；适用时客户端 CPU 达到单核 90% 或事件循环最大 lag 达到 100 ms 发出警告。这只是瓶颈启发式，不能证明客户端或后端没有瓶颈。闭环负载存在协调遗漏，其吞吐只是所选并发下的结果，不代表服务器饱和容量。比较独立轮次和原始记录，不只比较平均值。

参考 nginx 为两 worker、`sendfile on`、`open_file_cache off`、上游 keepalive 32、关闭代理 buffering 和 access log；这些设置随记录保存。本地标签 `nginx:1.28-alpine` 解出的二进制实际为 1.28.3，不是 1.28.0；动态 TLS 库版本以执行记录为准。
