# 011 验证数据

正式五模式、三重复、每轮 30 秒矩阵执行一次退出 0；独立核验逐行检查 7,306,995 条请求、135 个文件、1,100,820,560 bytes，结果通过。全部 15 轮有 client warning，闭环 RPS 不是容量。all 模式的日志及 span 队列丢弃原样保留；禁用 metrics 的模式计数不可用，不以 0 填充。

- `quality.log`、`quality-manifest.json`：红绿回归、fmt/check/clippy、176 个不同测试及原生超时 Collector 专项。`final-quality.log` 是无 emoji 测试样本收尾后的完整 176 项重验；`final-test-fixture-delta.json` 证明仅 cfg(test) 四字节样本变化，生产前缀字节相同。
- `release.log`、`release-manifest.json`、`example-*`、`example.log`：发布二进制及可观测性示例实际检查。
- `core-report.json`、`trace-ab.json`、`trace-c1.json`、`trace-c2.json`：有界日志/指标、真实 TLS WS/SSE 与 Reload、HTTP 错误 peer、同 PID Collector 恢复；各 scope 不互相冒充。
- `h2-*`、`timeout-*`：官方 Collector 原始日志与 local-root 字段核验；HTTP timeout 的 504 不冒充取消上下文已发送的 status。
- `pilot-*`、`full-*`、`independent-raw-check-*.json`：原始清单、完整摘要及独立量化核验。重连、预热边界、lag 原始观测和底层 CPU ticks 无法从保留 CSV 独立重构。
- `dependencies.json`、`collector.json`、`provenance.json`：来源、版本、哈希及信任限制。Collector manifest 的 `executed=false` 是提取时状态，随后实际 version/validate 和互操作日志另存，不应将旧字段当作最终未运行。
- `fail-*`、`static-regression-red.log`、`static-race-diagnostic.json`：保留先前失败及有意红测；最终成功不抹除历史。
- `reviews.json`、`evidence-files.json`：复审范围与所提升外部文件的字节/SHA 对照。

大型逐请求 gzip、完整访问日志和资源 CSV 留在 `/var/tmp/rill-011.nfYINU/artifacts/{pilot,full}/`，不复制进 Git。正式原始文件约 1.10 GB，不仅是 gzip；该外部目录不是可移植或永久存储担保。synthetic 私钥未保留或加入 Git。Git 中只保存小型摘要、清单、必要日志及核验报告；`evidence-files.json` 可与外部原始文件核对，文件字节原样保留。

运行方法、隔离条件和结果口径见 [011 验证报告](../../011-observability.md) 与 [benchmark README](../../../../scripts/bench/README.md)。SDK 和 Collector 版本由父流程从锁文件及已核发行记录提供；两个二进制实际哈希由 runner 匹配，不从二进制提取 SDK 包元数据。指标快照包含 readiness/预热/异步活动且在 shutdown 前，采样不证明全部时序、任意代次持有上界或长期资源无泄漏。AS 不是 RSS 上限。
