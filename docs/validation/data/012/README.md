# 012 证据

`retained-files.json` 记录从外部提升文件的来源、字节数和 SHA-256，不包含其自身、本说明及本地生成的 `reviews.json`、`final-source-equivalence.json`。后两者分别记录代理复审/执行索引，以及最终门禁输入、基线和工作树中 103 个 Rust/Cargo 文件的字节一致性。完整原始数据保存在 `/var/tmp/rill-012.zsDBix/artifacts/`；未提交二进制、大型请求记录或测试私钥。

当前保留基线质量及双版本 release 构建、二进制来源、组合场景、systemd 与 Podman 的结果和 manifest。组合、部署 manifest 中的其他日志路径指向外部原始证据，不代表所有日志均已提交。

正式 162 轮在线测量、独立原始核验及最终质量门禁均已通过。独核报告及完整原始 manifest、最终质量日志也已保留；重试和 client-limit warning 不得解释为零传输错误或容量证明。性能差异、最终复审和完成状态以相应验收报告为准。
