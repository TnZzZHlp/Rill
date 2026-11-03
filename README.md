# Rill

Rill 是 Linux 上的 HTTP/HTTPS 反向代理和静态文件服务器，支持多域名、KDL 配置及通过本地控制 Socket 执行配置和证书 Reload。

## 构建

需要 Rust Stable 1.95 或更新版本（edition 2024）、C 工具链、make、Perl、CMake 和 pkg-config（锁定依赖中的 libz-ng 使用 CMake 构建）：

```sh
cargo build --release --locked
```

可执行文件位于 `target/release/rill`。单二进制不表示完全静态链接：Pingora 的 OpenSSL 构建默认使用 vendored 源码，目标二进制仍可能依赖 glibc、动态加载器及其他共享库。部署时检查实际产物的 ELF interpreter 和 NEEDED 条目，并在目标系统提供相应运行库。HTTPS 上游还需要可用的 CA 信任资源。

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

`check` 验证配置及本地资源，但不监听、不联系运行实例，也不验证 DNS、上游连接或上游 TLS 信任。Reload 重新读取实例启动时的配置入口并等待实际结果；成功输出请求 ID 和代次。监听地址或控制 Socket 路径改变需要重启。

退出码 0 表示成功，1 表示配置、准备或服务端明确失败，2 表示命令参数错误，3 表示管理通信失败、超时或结果未知。结果未知不等于操作未发生；不要自动重试，应先检查日志和实际服务状态。

服务通过 SIGTERM 或 SIGINT 停止接受新连接并最多排空 30 秒；Reload 不是 SIGHUP 或重启。管理 Socket 目录须由服务有效 UID 拥有且权限为 0700，Socket 为 0600；服务端协议只允许服务 UID 和 root，但客户端还会验证服务端 UID。不要向不可信用户开放服务 UID 或 Socket。

HTTPS 示例 `examples/deploy/https.kdl` 需要管理员自行提供证书及私钥，不要将私钥提交到仓库。默认使用高端口，由外部组件转发低端口。

## 文档

- [配置和 CLI](docs/configuration.md)
- [架构](docs/architecture.md)
- [systemd 部署](deploy/systemd/README.md)
- [Podman 部署](deploy/podman/README.md)
- [性能测试](scripts/bench/README.md)
- [008 P0 验证记录](docs/validation/008-p0-validation-deployment.md)
- [008 部署验证记录](docs/validation/008-deployment.md)

P0 功能包括静态 TLS、代理、静态文件和 Reload；P1 可观测性增强和 ACME 不属于当前交付。性能和容量取决于实际环境，需以可复现的基准结果为准，不作预设承诺。部署示例尚待目标环境验证，不代表部署验收已通过。
