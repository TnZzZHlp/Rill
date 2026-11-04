# 012 受控部署验证

本文记录本次 systemd 与 Podman 部署 fixture；012-T03/AC03 已按限定环境验收，不代表生产部署保证。主验收索引为 [012-release-validation.md](012-release-validation.md)；性能由后续独立 `012-performance.md` 记录。T02 的正式 162 轮测量、独立原始核验及差异报告已验收；最终交付复审 `633d7505` 已另行通过，父流程已验收 G07；不将本文的限定环境结果推广为未测部署保证。关键结果及 manifest 已保存在 `docs/validation/data/012/`，逐文件来源、大小和 SHA-256 见其中 `retained-files.json`；完整命令日志仍在外部临时目录。

下文 `A` 表示 `/var/tmp/rill-012.zsDBix/artifacts`。本次未联网、安装或拉取镜像。实际二进制为以 HEAD `a1bf0314682b9d7e1e6a7f89d0d57b2e2e72755a` 构建的 `A/rill-current`，21,796,728 字节，SHA-256 `a97017b8af2f6df74dffc70a908770f66fc7a99324d633e283a5447869a4d523`；见 `A/rill-current-manifest.json`、`A/source-baseline-manifest.json` 和 `A/012-build-current.log`，不是 011 的旧二进制。Rust/Cargo 使用 1.99.1，未动态验证 MSRV 1.95。两个环境的 check 只验证配置及准备，不验证 DNS 解析或连接上游；真实代理请求另行验证。

## 实际环境与生产示例的差异

| 维度 | 仓库生产示例 | 本次受控 fixture |
|---|---|---|
| systemd 身份和隔离 | `deploy/systemd/rill.service` 使用 `rill` 账户、`RuntimeDirectory`、`PrivateTmp`、`ProtectSystem=strict` 等；不含 PrivatePIDs | root 可信 launcher 用系统 `unshare`/`setpriv` 准备环境，Rill 为 UID/GID 65534、所有 capability 为 0、NoNewPrivs；私有 network 和 PID，配置、www、usr 只读，专用 `/run/rill` 可写 0700、socket 0600。不是原 unit 逐字执行，也不创建持久账户 |
| systemd 资源 | unit 默认 512 MiB、CPUQuota 200%、TasksMax 64、nofile 4096 | cgroup 512 MiB、CPU 200%、64 tasks，CPU 6、7；客户端 AS 512 MiB、CPU 30 秒，root controller AS 2 GiB、CPU 240 秒、文件 32 MiB、总 timeout 360 秒 |
| Podman 构建 | `deploy/podman/Containerfile` 和 README 使用预先构建二进制、指定本地 base；README 另有未执行的 rootless 指南 | rootful Podman 使用已有 Debian base ID `394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853`、digest `sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4`，`podman build --network=none --pull=never --build-arg BASE_IMAGE=sha256:394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853 --iidfile ... --label org.rill.validation.nonce=...` 构建无 tag 镜像；完整 argv 和结果见 `A/podman/results.json` |
| Podman 运行 | README 的 rootful 示例使用专用网络并按需发布端口；rootless 使用 keep-id | UID/GID 10001、capability 0、NoNewPrivs、只读根和 `network=none`，不发布端口；配置、www 和 CA 只读，run 可写 0700/socket 0600。512 MiB、2 CPU、64 tasks、CPU 6、7，客户端同样有界；受限客户端加入目标网络、在隔离 PID 环境运行并清除不安全环境变量，不捕获宿主 root 密钥。实际 `podman create` 全 argv 见 `A/podman/results.json` 的 `006-container-create` |

这些验证不证明 rootless、所有发行版或生产账户可用；也不以 `network=none` 代替生产网络和对外监听验证。

## systemd full

`A/systemd/results.json` 的完整命令、退出码与 `command-NNN.log` 为操作证据；`A/012-systemd.log` 记录本次运行。最终 full 退出 0，manifest 的 `scenario=full`、`passed=true`，`basic_passed=false` 是 full 分支字段语义，不表示 full 失败，也不声称另跑过 basic。manifest 收录 54 文件、68,809 字节；父流程核对各文件 SHA，manifest 自身 SHA-256 为 `970ce6a2f965bd46f7b4fb5fe3f67aeeb89f24485ec4b5d558aa75b09f71d411`。固定私钥 PEM 标记扫描不是所有秘密均不存在的证明。

| 操作与预期 | 实际与定位 |
|---|---|
| 专用 unit `systemctl start rill-012-systemd-zsdbix.service`，以服务身份 `rill check --config /etc/rill/config.kdl`；TLS 和代理可用 | start/check 成功；真实 TLS backend 接收到 `/proxy` 和 Host `example.test`，见 `results.json`、`backend.log` |
| watch 导入 snippet/profile，cache-control max-age 10→20；随后通过 socket 手动 reload | 自动收敛至 generation 2；手动 reload 退出 0、generation 3。采样窗口及各响应 tuple 见 `results.json`，不声称证明 coordinator 空闲 |
| 坏语法、证书/私钥不匹配、修改启动期 cache 预算后 reload；应拒绝并保留旧 tuple | 各退出 1，分别为 `CONFIG_SYNTAX`、`TLS_INVALID`、`RESTART_REQUIRED`；响应正文 hash、cache-control、证书 hash 与 generation 的观察记录见 `results.json` 及 `command-026.log`、`command-035.log`、`command-043.log`。恢复配置后回到 max-age 10、generation 8 |
| 非服务 UID 1 访问控制 socket | reload 退出 3，见 `command-054.log`；该码表示通信失败或结果未知，不证明一般情况下 reload 一定未发生 |
| 下载在途时停止 unit，等待排空与清理 | 16 MiB 大写 L 完整接收，SHA-256 `289184e1081dba91206603d04683de839d8cceeb3bef6a56badf3dc904bb4043`；`Result=success`、`ExecMainStatus=0`、inactive，socket 与专用 unit 消失。见 `download.log`、`command-063.log`、`command-064.log` 和 `results.json` |

## Podman full

`A/podman/results.json` 记录每个固定 argv、退出码及对应 `NNN-*.stdout`/`.stderr`；`A/012-podman.log` 为运行摘要。最终 manifest `state=complete`、`basic_passed=true`、`full_passed=true`，没有 failure reasons，收录 206 文件、426,349 字节；父流程核对各文件 SHA，manifest SHA-256 `a0e90de10f901d100891872d3950361dcfd68c20a9aa8d731e441a651863f46d`。这是修正 setup 后获批的单次重跑，不把首次失败覆盖成通过。

| 操作与预期 | 实际与定位 |
|---|---|
| 从上述缓存 base 构建，按镜像 ID 创建并启动隔离容器；以容器服务 UID 执行 `rill check --config /etc/rill/config.kdl` | build、create、start、check 均退出 0；最终镜像 ID `b0b16f0997c1fb0c4c2095d6573ca9cd815b8cbce8043d21f2fca98bcf452c9f`，见 `004-build`、`006-container-create`、`010-rill-check` 输出及 results |
| 实际 TLS proxy、Host 和 loopback metrics | 代理 probe 验证下游 TLS 和正文，真实 backend 收到 Host `example.test`；metrics HTTP 200 且计数 5→6。见 `012-backend.stdout`、`014-probe-proxy.stdout`、`015-probe-metrics.stdout` 及 results |
| presampler 先确认旧 tuple，再将 max-age 10→20 | ACK 后才变更；watch 从 generation 1→2，三次采样稳定 0.649 秒，不证明 coordinator idle。手动 `rill reload --socket /run/rill/control.sock --timeout 35s` 退出 0、generation 3。见 `020-watch-cache.stdout`、`028-reload.stdout` 和 results |
| 语法错误、证书/私钥不匹配、cache 启动项变更与恢复 | 三次 reload 各退出 1，依次 `CONFIG_SYNTAX`、`TLS_INVALID`、`RESTART_REQUIRED`，旧 tuple 观察见 results；UID 1 reload 退出 3。`037-reload.stderr`、`049-reload.stderr`、`069-reload.stderr`、`079-reload.stderr` 为各自证据；恢复 fixture 后的 config、profile、证书和 key hash 与修改前一致，见 `017-probe-fixture-hashes.stdout`、`096-probe-fixture-hashes.stdout` |
| 在途大文件时 `podman stop --time 35`，随后清理本次资源 | 16 MiB 大写 L 的 SHA-256 同上，Rill 退出 0，socket 和 cgroup 消失；按捕获的本次容器 ID 和 nonce 镜像 ID 删除，`podman rm` 与 `podman rmi` 均退出 0。见 `088-download.stdout`、`090-drain-stop`、`100-container-remove`、`102-image-remove` 和 results |

## 失败记录及边界

| 保留的原记录 | 原因与经授权的修正 |
|---|---|
| `A/systemd-initial-pgrp0-error/`、`A/012-systemd-initial-pgrp0-error.log`、`A/systemd-initial-pgrp0-diagnostic.json` | 初始通用 parser 拒绝合法内核 PG 0 元数据；改为 owned leader guard 且允许非负值，不放宽身份校验。该早期目录只有 results，不能凭空声称有 manifest 或 journal |
| `A/systemd-invalid-profile/`、`A/012-systemd-invalid-profile.log`、`A/012-systemd-invalid-profile-journal.log` | profile 不支持 cache-control；改为 imported snippet/use-snippet，新 public KDL 由两个 UID 的 Rill check 各退出 0，见 `A/012-role-fixture-v2-check.log`；原 manifest 保留失败 |
| `A/systemd-header-case-error/`、`A/012-systemd-header-case-error.log`、`A/012-systemd-header-case-error-journal.log` | 客户端字典按 `Cache-Control` 大小写查找；HTTP header 名不区分大小写，规范化名称，不修改预期值；原 manifest 保留失败 |
| `A/podman-build-filelimit-error/`、`A/012-podman-build-filelimit-error.log`、`/var/tmp/rill-012.zsDBix/podman-build-context-filelimit-error` | 首次受控 CLI 文件限制 1 MiB 拒绝约 21.8 MiB 二进制 COPY；重跑只将 CLI 文件限制改为 32 MiB，stdout 读取硬拒超过 1 MiB、stderr 限 32 MiB，target probe 的文件限制仍为 1 MiB。原失败目录和 context 保留 |

首次 Podman build 未捕获 IID，因此没有盲删镜像。后续只读诊断：确切失败 overlay layer `f1d35...` 不存在，storage 映射无匹配，确切 nonce 镜像查询为空；`ps --external` 忽略 label filter 并返回未知旧 ID，未删除它们。见 `A/podman-initial-build-cleanup-diagnostic.json`。这不是全部历史 storage 干净的证明；最终运行也只清理本次拥有并捕获 ID 的资源。
