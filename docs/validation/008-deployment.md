# 008 受控部署验证记录

## 状态与共同环境

修复后完整标准基准、原始核验及阶段最终交付独立复审均已完成。本报告仅证明所述受控部署，不代表任意生产环境或 rootless 路径已验证。两组最新测试均使用 `/var/tmp/rill-008.zWfZ2O/artifacts/rill-nodelay`，SHA-256 `c6be369b6d7b122924b5886421daf459491020eb8ff499e158f71d29b2484ac2`。旧部署证据位于 `artifacts/deployment-before`，不作为新版结果。结构化摘要及外部结果文件哈希见 [deployment-summary.json](data/008/checks/deployment-summary.json)。

内核 7.2.11-x64v3-xanmod1，systemd 257.13-1~deb13u1，Podman 5.4.2，Rust/Cargo 1.99.1；最低 Rust 1.95、edition 2024。以下命令描述真实操作及关键参数，完整编排、输入和逐项结果以外部 artifacts 为准，非逐字可直接用于生产的脚本。

## systemd

使用外部 `008-systemd-fixture.service` 和 `008-systemd-harness.txt` 的受控 fixture，执行启动、`rill check`、经 CA 与 hostname 验证的 HTTPS、同 UID CLI 及 `systemctl reload`，再执行 SIGTERM 排空。启动/check 成功，HTTPS 200。同 UID CLI 授权；root 原始协议授权。UID 1 通常被文件系统拒绝；仅在 fixture 将目录/socket 从 700/600 放宽为 711/666 后得到真实协议 `UNAUTHORIZED`，该拒绝请求未推进代次，随后恢复 700/600 并由同 UID 请求确认。坏语法 reload 退出 1，旧路由 age 10 不变；`systemctl reload` 收到真实响应后 age 20，generation 连续至 6。

16 MiB 下载中 SIGTERM，排空 15.992581536 秒，完整 SHA-256 `289184e1081dba91206603d04683de839d8cceeb3bef6a56badf3dc904bb4043`，客户端及服务退出 0，systemd Result=success。

Rill 以 UID/GID 65534 运行，CapInh/Prm/Eff/Bnd/Amb 均为 0、NoNewPrivs=1，私有 PID namespace 中为 PID 1，私有网络仅 loopback，只读 RootDirectory，runtime/tmp/var_tmp 各 16 MiB；Memory 512 MiB、Tasks 64、CPUQuota 200%、affinity 6,7、AS 2 GiB、CPU 120 秒、文件 64 MiB、wall 180 秒。可信 root launcher 只执行 unshare、setpriv、env 等系统工具建立隔离，Rill 本身绝不以 root 执行。

fixture 的 `User=root` 仅用于可信 setup；RuntimeDirectory 由父代理准备限额 tmpfs，`PrivateTmp=false` 由限额 tmpfs 替代，`ProcSubset=all` 供工具使用。systemd 257 下 RootDirectory 与 PrivatePIDs 组合失败；未通过放宽 Rill 最终 capability 绕过。生产 unit 不含 PrivatePIDs，使用预先准备的 `User=rill`；本次测试未创建宿主账号，也未在宿主执行正式 sysusers 安装指南，fixture 不能逐字视作生产模板已验证。unit 和 runtime tmpfs 已清理。

证据：外部 `008-systemd-results.json`、`008-systemd-journal.log`、`008-systemd-fixture.service`、`008-systemd-harness.txt`，位于 `/var/tmp/rill-008.zWfZ2O/artifacts/`。

## Podman

rootful Podman 中 Rill 使用非 root UID/GID 10001、capability 全零、no-new-privileges，PID namespace 中为 PID 1。基底为已缓存 Debian trixie-slim，ID `394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853`，digest `sha256:1e1a22175fbf1b32157ddaf0219e1db4e5401fab2b94ed285add64d18587cd2d`；新镜像 ID `31480fb4e829807300b287cee9625c01b2ea247c3aa8c53e4f0a7155afd87296`，已清理。

运行参数包括 `--network=none`、不 publish、只读 root、`--cap-drop=ALL`、no-new-privileges、memory 512 MiB 且 memory+swap 总计 512 MiB、pids 64、CPU 2、cpuset 6-7、nofile 4096、CPU 120 秒、fsize 64 MiB，以及 tmp 和 runtime 各 16 MiB。Podman 不支持此处 AS ulimit，未声称 AS 2 GiB。配置、证书、root 和 CA 文件只读挂载；CA 位于 `/etc/ssl/certs/ca-certificates.crt`，配置 `SSL_CERT_FILE`/`SSL_CERT_DIR`。可信 hosts 只将 `example.test` 指向 loopback，`/api/status` 经 `/api/*` 路由转发真实 TLS 后端 9443，后端验证 SNI `example.test`；前后两次 HTTPS 静态和上下游均验证并返回 200。

`check` 退出 0；reload 到 generation 2；坏语法返回 1，旧 age 20 仍可用；恢复为 generation 3；`podman exec --user 1` CLI 返回 3 被拒，服务 UID 成功至 generation 4。16 MiB 排空 16.044472844 秒，哈希同 systemd，进程退出 0、Socket 移除。

镜像实测 glibc 2.41；ELF 有 interpreter、libgcc_s、libm、libc 和 loader，最高符号 GLIBC 2.38，并非全静态。OpenSSL 3.6.3 vendored，无动态 ssl/crypto NEEDED，但 HTTPS 上游仍需 CA。仅清理本阶段容器及镜像新层；Debian/nginx 本地缓存未动，无 pull/install 或既有账户、服务更改。rootless 指南未实际执行，不声称 rootless 已验收。

证据：外部 `008-podman-results.json`、`008-podman-server.log`、`008-podman-build.log`，路径同上；生产模板和操作说明见 `deploy/podman/README.md` 与 `deploy/systemd/README.md`。
