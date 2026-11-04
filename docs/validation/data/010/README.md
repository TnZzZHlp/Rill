# 010 验证数据

- `summary.json`：最终重跑的 21 轮正式测量，三重复、每轮 30 秒；不含 pilot 或边界修复前的整批结果。
- `independent-raw-check-final.json`：独立逐行核验的计算值与资源峰值；最终 8,106,101 行窗口及关键统计通过，保留修复前单行违规定位。
- `raw-manifest.json`：最终完整 110 个文件的原始字节数和 SHA-256；完整内容保留于 `/var/tmp/rill-010.evlQY4/artifacts/full/`。
- `pilot-manifest.json`：独立 7 轮、每轮 2 秒的 pilot 文件清单；完整内容在同目录的 `pilot/`。
- `before-window-fix-manifest.json`：首批正式结果原样保留于 `full-window-boundary-before/`。该批有一条请求的 driver begin 超过 deadline 约 104 ns，不能当作最终正式结果；未删行或改 summary。
- `round-*-resources.csv`、`round-*-window.json`：最终每轮资源采样与精确 monotonic 窗口。CSV 保留原始 CRLF 字节，不能规范化后再声称与原 manifest 相同。
- `environment.json`：执行环境、release 哈希和源码一致性检查；`reviews.json`：独立复审关闭索引。
- `release-source-manifest.json`：release 冻结源码的文件哈希；`measurement-source-hashes.json` 区分旧测量源码与单次 begin 检查修复后的源码。实际 release 二进制不写入 Git。
- `valid-sidecars.json`：1 KiB fixture；生成时使用已有 libbrotli 编解码及 gzip 核对正文。驱动运行时只独立解码 gzip，br 比较外部已验证的字节。
- `010-*.log`：最终门禁、红色回归、真实挂载、release/示例和正式测量的执行记录。红色回归预期退出 101，其余列出的最终执行退出 0。

逐请求 gzip CSV 不纳入 Git。最近秩延迟、请求与正文总数、窗口完成数及 late/drain 可结合请求 CSV 和 window JSON 复算；预热边界、重连历史与在线事件循环 lag 不完全包含在测量 CSV 中，不冒充全字段可重建。client 资源采样仅 driver 自身，不计服务子树。

资源采样不证明所有时刻的峰值；RSS 可重复计算共享页，HWM 为历史值，无 PSS/cgroup 内存证据。缓存正文额度与 owner 生命周期由核心测试验证；基准没有采集实际 used 或命中率。全部正式轮次存在客户端限制警告，不把闭环吞吐当成容量或宣称稳定缓存收益。

最终结论和 OS 隔离约束见 [验证记录](../../010-static-enhancements.md)。外部原始文件需要继续归档，Git 中的摘要不替代原始数据。
