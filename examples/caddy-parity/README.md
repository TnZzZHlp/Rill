# 隔离组合示例

此示例仅监听 loopback，不含生产域名、证书或凭据。静态/SPA 使用 `public/`；`/api/*` 指向独立 HTTP/1 后端 127.0.0.1:18081，`/grpc.example.Service/*` 指向通用 h2c gRPC 后端 127.0.0.1:18082。后端须自行提供；`check` 不验证连接。动态代理只允许合规公网目标，不允许把 loopback 后端当成动态目标；它和 DAV 使用同一测试令牌，仅用于演示，不能直接用作生产权限设计。

在独立临时目录操作，不要在源码目录生成凭据：

```sh
work=$(mktemp -d /var/tmp/rill-parity-XXXXXX)
cp -R examples/caddy-parity/. "$work/"
mkdir -m 700 "$work/runtime" "$work/runtime/log" "$work/runtime/dav"
(umask 077; head -c 32 /dev/urandom | base64 > "$work/runtime/token")
rill check --config "$work/config.kdl"
rill run --config "$work/config.kdl"
```

另一个终端可用 `curl -H 'Host: parity.test' http://127.0.0.1:18080/`，或加 `Accept: text/html` 请求 `/navigation` 验证 SPA。`OPTIONS /api/test` 返回 CORS 204；普通 API 和 gRPC 调用需要对应后端。静态响应的 ETag 可用 `If-None-Match` 验证 304。DAV 请求读取 `runtime/token` 作为 `X-Dav-Token`，不要在共享终端、日志或命令历史暴露令牌；`/dav/` 前缀被剥离。动态代理以认证后 `/fetch/` 加一次 URL 编码的完整 HTTP(S) 目标访问，HTML 响应示例将 `old.example.test` 替换为 `new.example.test`，规则不保证目标 HTML 一定含该文本。

控制操作使用 `rill reload --socket "$work/runtime/control.sock"`；结束后停止本次服务，再删除本次临时目录。端口冲突时在隔离副本内调整。日志目录必须由服务 UID 拥有且 0700，所有祖先必须可信，组或其他用户可写的祖先须有 sticky 位，整个路径拒绝 symlink。DAV root 必须允许服务 UID 写入、xattr 和 O_TMPFILE；日志文件创建也要求 O_TMPFILE/linkat，不回退到不安全创建。systemd 须显式开放 DAV 和日志目录写权限，容器须使用可写挂载。实际部署前单独审查访问控制、后端、TLS 和路径；不得把本例视为生产切换或容量承诺。

PUT 提交后的同步失败或连接中断可能导致写入结果未知，先核验目标再决定恢复，不自动重试。日志磁盘信用仅限单进程寿命，关闭的五秒限制仅覆盖用户态等待，D 态 I/O 可延迟进程回收。详细约束见 `docs/configuration.md` 与 `docs/architecture.md`。
