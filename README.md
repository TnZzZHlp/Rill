# Rill

Rill 是 Linux 上的 HTTP/HTTPS 反向代理和静态文件服务器，支持多域名、KDL 配置、Snippet 路由复用，以及通过本地控制 Socket 执行配置和证书 Reload。自动文件监听可显式启用，默认关闭。

## 构建

需要 Rust Stable 1.95 或更新版本（edition 2024）、C 工具链、make、Perl、CMake 和 pkg-config（锁定依赖中的 libz-ng 使用 CMake 构建）：

```sh
cargo build --release --locked
```

1.95 是声明的 MSRV，并非此处声称已用该最低版本实际构建；实际工具链为 1.99.1。依赖已完整缓存时可使用 `cargo build --release --locked --offline`；否则下载须先获得受控批准。可执行文件位于 `target/release/rill`。单二进制不表示完全静态链接：Pingora 的 OpenSSL 构建默认使用 vendored 源码，目标二进制仍可能依赖 glibc、动态加载器及其他共享库。部署时使用 `readelf` 检查实际产物的 ELF interpreter 和 NEEDED 条目，并在目标系统提供相应运行库。HTTPS 上游还需要可用的 CA 信任资源。

## 快速开始

`examples/deploy/config.kdl` 使用 8080 端口，站点为 `example.test`，静态根目录为 `/srv/rill/www`。先创建并填充静态根目录，将示例部署为 `/etc/rill/config.kdl`，并按部署说明准备服务身份、读取权限和私有控制 Socket 目录。以该服务身份运行：

```sh
rill check --config /etc/rill/config.kdl
rill run --config /etc/rill/config.kdl
```

另开终端，使用配置的 Host 访问，并以同一服务身份请求 Reload：

```sh
curl -H 'Host: example.test' http://127.0.0.1:8080/
rill reload --socket /run/rill/control.sock --timeout 35s
```

systemd 部署推荐 `systemctl reload rill`，也可使用 `sudo -u rill rill reload --socket /run/rill/control.sock --timeout 35s`。客户端只信任自身 UID 或 root 的服务端；root CLI 连接非 root 服务会拒绝其 peer，不能假定 `sudo rill reload` 可用。

`check` 验证配置及本地资源，但不监听、不联系运行实例，也不验证 DNS、上游连接或上游 TLS 信任。Reload 重新读取实例启动时的配置入口并等待实际结果；成功输出请求 ID 和代次。监听地址、控制 Socket 路径或 watch 启动设置改变需要重启。

退出码 0 表示成功，1 表示配置、准备或服务端明确失败，2 表示命令参数错误，3 表示管理通信失败、超时或结果未知。结果未知不等于操作未发生；不要自动重试，应先检查日志和实际服务状态。

服务通过 SIGTERM 或 SIGINT 停止接受新连接并最多排空 30 秒；Reload 不是 SIGHUP 或重启。管理 Socket 目录须由服务有效 UID 拥有且权限为 0700，Socket 为 0600；服务端协议只允许服务 UID 和 root，但客户端还会验证服务端 UID。不要向不可信用户开放服务 UID 或 Socket。

复用示例 `examples/reuse/config.kdl` 使用本地静态文件，不启用 watch；`examples/reuse/watch.kdl` 单独展示显式启用。运行前将其 Socket 占位路径改为管理员准备的服务 UID 所有、权限为 0700 的私有目录中的路径。

HTTPS 示例 `examples/deploy/https.kdl` 需要管理员自行提供证书及私钥，不要将私钥提交到仓库。默认使用高端口，由外部组件转发低端口。

## 文档

- [配置和 CLI](docs/configuration.md)
- [架构](docs/architecture.md)
- [systemd 部署](deploy/systemd/README.md)
- [Podman 部署](deploy/podman/README.md)
- [性能测试](scripts/bench/README.md)
- [三目标 Podman 性能对比](docs/validation/bench-three-way.md)
- [008 P0 验证记录](docs/validation/008-p0-validation-deployment.md)
- [008 部署验证记录](docs/validation/008-deployment.md)

已交付静态 TLS、代理、静态文件、Reload、Snippet、可选 watch、静态增强和访问日志。访问日志默认关闭，主入口保持精简；运行日志与访问日志共用后台日志线程。ACME 和 HTTP/3 不属于当前目标。历史验证范围见 008–012 记录；其中指标和追踪的历史结果不代表当前仍支持这些功能。闭环客户端受限 RPS 不是容量，不能据旧记录承诺长期稳定或性能领先。

## 完整组合示例

`examples/release/` 将入口、共享 snippet、三个域名的 site 和静态内容分开。保留目录结构复制后，将 `config.kdl` 的控制 Socket 改为服务 UID 拥有的 0700 私有目录内路径；默认 `/run/rill/control.sock` 仅在管理员已准备该目录时可用。以服务身份执行：

```sh
rill check --config /path/to/release/config.kdl
rill run --config /path/to/release/config.kdl
curl -H 'Host: release.test' http://127.0.0.1:18080/
rill reload --socket /run/rill/control.sock --timeout 35s
```

上例的 curl 和 reload 在另一个终端执行，Socket 参数须与修改后的配置一致。`/api/*` 转发给同网络命名空间的 `127.0.0.1:18081`，需要管理员另行提供后端；`/api` 不匹配该前缀。三个域名共享 snippet，root 相对 snippet 定义文件解析；Profile 使用 `use`，snippet 使用 `use-snippet`。静态 fallback 启用 SPA 和预压缩，但未提供压缩文件：管理员可提供修改时间不早于原文件的真实 `.gz` 或 `.br`，缺失时可回退 identity。本示例只有 HTTP；HTTPS 另见 `examples/deploy/https.kdl`，由管理员提供证书和私钥。

`check` 不做 DNS 查询、不 bind 端口。实例以启动时的主配置路径为入口构建快照，reload 重新读取该入口，不是 CLI 按路由逐项加载。监听、控制路径、watch 和缓存预算是启动项，修改须重启。其他配置、snippet、TLS 和策略按同一事务验证发布，访问日志开关和采样可热更新。详见[配置](docs/configuration.md)。

兼容已有 P0 配置：不添加新块时保持默认语义，但关闭访问日志并不意味着没有后台日志线程。KDL 2 布尔值须写 `#true`/`#false`，未知字段严格报错并指向来源。watch 默认关闭；开启后仅跟踪配置、导入和 TLS 依赖，不主动跟踪静态内容树。
