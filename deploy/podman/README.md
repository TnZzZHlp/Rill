# Podman 部署

rootful 非 root UID 10001 的受控预检已覆盖启动、check、HTTPS、Reload、语法失败回滚及 16 MiB 在途 SIGTERM 排空；范围和结果见 [008 验证记录](../../docs/validation/008-p0-validation-deployment.md)，不代表正式生产部署已通过。rootless 指南尚未执行。示例需要预先构建的、与目标架构和镜像兼容的 `rill` 二进制及本地 Debian trixie-slim 镜像。构建上下文只包含 `Containerfile` 和名为 `rill` 的 release 二进制；不安装包，不复制配置、证书、私钥或静态内容。

记录二进制 SHA-256、基础镜像 digest、最终镜像 ID 和目标环境。通过 `BASE_IMAGE` 传入已缓存的固定 digest：

```sh
podman build --network=none --pull=never --build-arg BASE_IMAGE=docker.io/library/debian@sha256:REPLACE_WITH_CACHED_DIGEST -t localhost/rill:local CONTEXT
```

`CONTEXT` 指向专用构建上下文；digest 占位值须替换为实际已缓存值。检查 release 二进制的 ELF interpreter 和 NEEDED 条目，并验证镜像提供实际所需的加载器和共享库。Debian 的 `/bin`、库目录可能是符号链接，检查解析后的目标及 glibc loader；vendored OpenSSL 不表示完全静态或无运行依赖。

配置和静态根以只读方式挂载。证书和私钥由管理员提供并只读挂载，绝不打包进镜像。HTTPS 上游需要 CA 信任资源：以下示例将实际存在的 CA bundle 只读挂载到 `/etc/ssl/certs/ca-certificates.crt`，并设置 `SSL_CERT_FILE`。须验证其内容、路径和运行身份读取权限。

## rootless

推荐 rootless，以 `--userns=keep-id` 和调用者 UID/GID 运行。`RILL_RUN_DIR` 必须由宿主调用者拥有且权限 0700；不要使用 sudo 或将其 chown 给容器内部固定 UID。其他文件须对映射后的运行身份可读。变量均为管理员准备好的绝对路径，`RILL_NETWORK` 是专用隔离网络：

```sh
podman run -d --name rill-local \
  --read-only --network "$RILL_NETWORK" \
  --publish 127.0.0.1:8080:8080 --publish 127.0.0.1:8443:8443 \
  --userns=keep-id --user "$(id -u):$(id -g)" \
  --cap-drop=ALL --security-opt=no-new-privileges \
  --memory=512m --memory-swap=512m --pids-limit=64 --cpus=2 --ulimit nofile=4096:4096 --stop-timeout=35 \
  --mount "type=bind,src=$RILL_CONFIG_DIR,dst=/etc/rill,ro" \
  --mount "type=bind,src=$RILL_WWW_DIR,dst=/srv/rill/www,ro" \
  --mount "type=bind,src=$RILL_RUN_DIR,dst=/run/rill,rw" \
  --mount "type=bind,src=$RILL_CA_FILE,dst=/etc/ssl/certs/ca-certificates.crt,ro" \
  --env SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
  localhost/rill:local
```

## rootful

管理员另行准备由宿主 UID 10001 拥有、权限 0700 的专用 `RILL_RUN_DIR`，并确保其他资源对 UID/GID 10001 可读。以下是独立 rootful 示例，不使用 rootless 的 keep-id 映射：

```sh
sudo podman run -d --name rill-local \
  --read-only --network "$RILL_NETWORK" \
  --publish 127.0.0.1:8080:8080 --publish 127.0.0.1:8443:8443 \
  --user 10001:10001 --cap-drop=ALL --security-opt=no-new-privileges \
  --memory=512m --memory-swap=512m --pids-limit=64 --cpus=2 --ulimit nofile=4096:4096 --stop-timeout=35 \
  --mount "type=bind,src=$RILL_CONFIG_DIR,dst=/etc/rill,ro" \
  --mount "type=bind,src=$RILL_WWW_DIR,dst=/srv/rill/www,ro" \
  --mount "type=bind,src=$RILL_RUN_DIR,dst=/run/rill,rw" \
  --mount "type=bind,src=$RILL_CA_FILE,dst=/etc/ssl/certs/ca-certificates.crt,ro" \
  --env SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
  localhost/rill:local
```

按实际 HTTP 或 HTTPS 配置发布相应端口；示例只发布宿主 loopback，不更改宿主全局 sysctl。HTTPS 示例需要自行提供证书和私钥。未配置站点文件日志时容器只需运行目录可写；配置文件日志时，管理员另行预建由映射后服务 UID 所有且模式 0700 的宿主日志目录，以 `--mount "type=bind,src=$RILL_LOG_DIR,dst=/var/log/rill,rw"` 显式挂载，并保证容器内祖先目录符合配置文档的规则。如环境确需临时目录，可额外指定 `--tmpfs /tmp:rw,noexec,nosuid,size=16m`。不要使用 0777 或放宽管理 Socket 权限。

`--network=none` 不能提供外部服务，适合受控隔离验证；共享专用私有网络命名空间也是可选测试方式。不能将 `--network=none` 与端口发布当成同时有效的生产部署。Linux `openat2`、`renameat2` 和可用且身份一致的 procfs `/proc/self/fd` 是硬条件；容器 seccomp、用户命名空间及 SELinux 标签须实测兼容。遇到不兼容应报告错误，不以 `--privileged` 绕过。

使用与启动相同的 Podman 身份执行以下命令；`podman exec` 默认使用容器配置的服务 UID，不要切换到 UID 0：

```sh
podman exec rill-local /usr/local/bin/rill check --config /etc/rill/config.kdl
podman exec rill-local /usr/local/bin/rill reload --socket /run/rill/control.sock --timeout 35s
```

服务端允许服务 UID 或 root，但 CLI 客户端仅信任自身 UID 或 root 的服务端；root CLI 连接非 root 服务会拒绝 peer。Reload 等待实际响应并返回对应退出码，不以容器重启代替。退出码 3 表示结果可能未知，不自动重试。停止时允许至少 35 秒以覆盖 SIGTERM 排空。验证后只删除本次命名的容器、专用镜像标签和专用网络，不运行全局 prune。

受控预检使用 Debian 13 trixie-slim 本地镜像，rootful 容器内以 UID 10001 运行，根只读、丢弃全部 capability、no-new-privileges，限制内存 512 MiB、内存与 swap 合计 512 MiB、pids 64、CPU 2 核并固定 CPU 6–7、nofile 4096；使用私有 `network=none` 且不发布端口。CA bundle 只读挂载至标准路径并设置 `SSL_CERT_FILE`；可信 hosts 仅将 `example.test` 映射到 loopback，实际检查 HTTPS 上游 127.0.0.1:9443 的 SNI 和 CA 验证。生产桥接网络示例与该 fixture 不同。具体镜像、glibc 和动态链接依赖以对应构建的验收记录及实际 ELF 检查为准；内存使用 cgroup 限制，不依赖本次 Podman 不支持的 `--ulimit as`。

## 组合配置与观测

`examples/release/` 将 watch、8 MiB 正文缓存和 observability 放在 server，将 SPA、预压缩和目录浏览放在静态 route。保留目录结构并修改控制路径，配置、站点和私钥对服务身份只读；watch 不需要写权限。watch 默认关闭，开启后挂载配置目录而非单文件 bind，以观察普通文件原子替换；它跟踪配置、导入和 TLS 依赖，不主动跟踪静态树。缺失 glob 父目录及 inotify 预算边界见[配置文档](../../docs/configuration.md)。

缓存默认 0，8 MiB 是正文 owner 生命周期的共享预算，不是 RSS、在途请求或元数据预算。预压缩和目录浏览默认关闭，错误页默认 none；管理员须提供不早于原文件的真实压缩 sidecar，缺失可回退 identity。目录浏览会公开相应可访问条目，不建议随意启用。监听、控制路径、watch 和缓存预算为启动项，改变返回 `RESTART_REQUIRED`；访问日志开关和采样可热更新，snippet、root、TLS 和策略同事务发布。不要用 SIGHUP 代替 reload。旧 P0 配置不写新块保持默认语义；KDL 2 布尔值须用 `#true`/`#false`，未知字段严格报来源。

组合示例的业务监听为容器内 `127.0.0.1:18080`；桥接网络中仅发布端口不能使外部访问此 loopback 监听。需要容器外访问时，管理员须按网络策略将业务监听改为容器接口可达地址（例如 `0.0.0.0:18080`），并发布对应的 18080 端口；这是启动项变化，需要重启。访问日志不含 URL、headers、客户端 IP，但管理员诊断可能包含路径和地址，opaque hash 不保证匿名化。运行日志与访问日志共享 4096 × 4 KiB 的 best-effort 队列，关闭最多等待 1 秒，不保证全部 flush；默认关闭访问日志仍有后台日志线程。详见[配置](../../docs/configuration.md)和[架构](../../docs/architecture.md)。

指标和 OTLP 追踪已移除，部署旧配置时删除 `metrics`、`tracing`、`trace-sample-per-mille`、`otlp-endpoint`，不再提供额外抓取端口或 Collector 集成。`SSL_CERT_FILE`/`SSL_CERT_DIR` 仍影响上游 TLS；配置、证书和私钥不得打包进镜像。资源限制示例不是容量承诺；rootless 仍是未执行指南，历史预检不保证当前生产部署。不要全局 prune、修改宿主 sysctl 或停止共享生产服务。
