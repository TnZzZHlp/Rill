# systemd 部署

受控预检已覆盖启动、check、HTTPS、Reload、语法失败回滚及 16 MiB 在途 SIGTERM 排空；范围和结果见 [008 验证记录](../../docs/validation/008-p0-validation-deployment.md)。这不代表正式生产部署已通过。需要兼容所用安全指令的 systemd、Linux `openat2` 和 `renameat2`，以及可用且身份一致的 procfs `/proc/self/fd`；不兼容时应报告错误，不能绕过安全限制。

构建并审查 release 二进制后，检查其 ELF interpreter 和 NEEDED 条目；目标系统须提供实际需要的动态加载器和共享库。默认 vendored OpenSSL 不意味着二进制完全静态，HTTPS 上游仍需要系统 CA 信任资源；确认系统 CA 路径及其内容真实可用，必要时为服务配置 `SSL_CERT_FILE` 或 `SSL_CERT_DIR`。

正式部署由管理员执行，以下步骤会创建持久服务账户，不属于受控测试步骤：

```sh
sudo install -m 0644 deploy/systemd/rill.sysusers /usr/lib/sysusers.d/rill.conf
sudo systemd-sysusers /usr/lib/sysusers.d/rill.conf
sudo install -m 0755 target/release/rill /usr/local/bin/rill
sudo install -m 0644 deploy/systemd/rill.service /etc/systemd/system/rill.service
sudo systemctl daemon-reload
```

发行版若使用其他 sysusers 目录，应按其约定安装。管理员准备 `/etc/rill/config.kdl` 和 `/srv/rill/www`，并使服务身份可读；示例配置可从 `examples/deploy/config.kdl` 复制。配置和静态内容由管理员维护，服务只读。证书目录可使用 `root:rill`、0750，私钥使用 `root:rill`、0640；目录链和证书文件也须允许服务身份读取。不要将真实私钥提交到仓库。`RuntimeDirectory=rill` 创建由服务身份拥有的 0700 `/run/rill`，程序创建 0600 控制 Socket。不要向不可信用户开放服务 UID 或 Socket。

默认示例监听 8080；HTTPS 示例监听 8443，需管理员提供覆盖 `example.test` 的证书和私钥。对外 80/443 可由外部组件转发。确需直接绑定低端口时，创建经审查的 systemd override，仅授予绑定能力，并验证本机低端口策略：

```ini
[Service]
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
AmbientCapabilities=CAP_NET_BIND_SERVICE
```

按环境调整 `MemoryMax`、`TasksMax`、`LimitNOFILE` 和 `CPUQuota`，记录实际值；示例值不是容量承诺。`ProtectSystem=strict` 和显式路径限制使配置和静态根只读、运行目录可写；本地挂载和安全策略须实际验证。

启动、检查状态、Reload 和停止由管理员使用 `systemctl start rill`、`systemctl status rill`、`systemctl reload rill`、`systemctl stop rill`。`ExecStartPre` 以服务身份运行 check；`ExecReload` 以服务身份通过控制 Socket 等待真实响应，不是 SIGHUP。服务端协议允许服务 UID 或 root，但 CLI 客户端仅信任自身 UID 或 root 的服务端：root CLI 连接非 root 服务会拒绝其 peer。手动调用应以服务 UID 执行，例如 `sudo -u rill rill reload --socket /run/rill/control.sock --timeout 35s`。退出码 3 时不能假定配置未发布，不自动重试。停止发送 SIGTERM，unit 等待 35 秒以覆盖最多 30 秒排空。

受控测试使用专用只读 `RootDirectory`、私有 PID/mount/net 命名空间和受限 scratch，身份覆盖为已有 `nobody:nogroup`。可信 root launcher 只使用系统工具 unshare/setpriv 搭建环境，不执行 Rill 代码；Rill 以 UID/GID 65534、所有 capability 集合为零及 NoNewPrivs=1 运行。可信父进程在 16 MiB tmpfs 预备 0700 运行目录，Socket 为 0600；`/tmp` 和 `/var/tmp` 各用 16 MiB tmpfs 替代 PrivateTmp。测试未运行 sysusers，未创建宿主持久账户。由于本机 systemd 257 的 RootDirectory 与 PrivatePIDs 组合失败，生产 unit 未加入 PrivatePIDs；测试也覆盖了 User、RuntimeDirectory、ProcSubset 和命名空间准备，不能称为生产 unit 逐字运行或证明每个发行版及正式账户均可用。fixture 覆盖与生产安装的差异见验收记录。测试结束只清理本次专用资源。

## 组合配置与运维

`examples/release/` 展示 server 内的 `watch`、`static-cache-bytes` 和 `observability`，以及静态 route 内的 `spa`、`precompressed`、`browse`；保留目录结构复制并调整 Socket 路径。watch 默认关闭，可手动启用；服务只需读取配置、站点和私钥，不应为了 watch 授予写权限。挂载配置目录而非单文件 bind，以便观察普通文件的原子替换；缺失 glob 的父目录和 inotify 预算及边界见[配置文档](../../docs/configuration.md)。watch 观察配置、导入及 TLS 依赖，不主动监听静态内容树。

缓存默认 0；示例的 8 MiB 是正文在 owner 生命周期内共享的预算，包括仍被响应持有的正文，不是 RSS、在途请求或元数据总预算。预压缩和目录浏览默认关闭，错误页默认 none；预压缩需要管理员提供不早于原文件的真实 sidecar，缺失可回退 identity。启用目录浏览会公开相应可访问条目，默认不建议随意开启。

监听地址、控制 Socket、watch 和缓存预算为启动项，修改返回 `RESTART_REQUIRED`；应重启而不是发送 SIGHUP。访问日志开关和采样可热更新；snippet、root、TLS 和策略随候选配置同一事务验证发布。KDL 2 布尔值为 `#true`/`#false`，未知字段严格报错并带来源。

访问日志不含 URL、headers 或客户端 IP，但管理员运行时诊断可能包含路径和地址；opaque hash 不保证匿名化。运行日志与访问日志共用 4096 × 4 KiB 的 best-effort 队列，关闭最多等待 1 秒，不能保证全部 flush；默认关闭访问日志仍有后台日志线程。详见[配置](../../docs/configuration.md)和[架构](../../docs/architecture.md)。

指标和 OTLP 追踪已移除，部署旧配置时删除 `metrics`、`tracing`、`trace-sample-per-mille`、`otlp-endpoint`；不再需要 Collector 或 OpenTelemetry 环境变量 override。`SSL_CERT_FILE`/`SSL_CERT_DIR` 仍影响上游 TLS。历史隔离预检不代表当前所有发行版或正式账户的生产验收。正式安装和持久账户由管理员操作，不停止共享生产服务、不修改宿主 sysctl。
