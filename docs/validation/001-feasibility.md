# 001 技术验证与复审记录

## 范围

本记录仅证明配置契约的语法选择、Pingora 扩展点及单根快照机制可行，不代表完整 Web 服务器或生产 Reload 已交付。生产入口仍为初始化程序。执行路径为当前仓库根目录，软件验证命令均由实施期间真实执行。

## 1. 环境与锁定版本

| 项目 | 实际值 |
| --- | --- |
| OS | Debian GNU/Linux 13.7 (trixie)，x86_64 Linux |
| Rust | rustc 1.99.1，stable-x86_64-unknown-linux-gnu |
| Cargo | 1.99.1 |
| 工程声明最低 Rust | 1.95；未声称在 1.95 工具链实际构建通过 |
| Pingora / pingora-core / pingora-openssl | 0.9.0 |
| KDL | 6.7.1，v2，span，无 v1-fallback |
| ArcSwap | 1.9.3 |
| Tokio | 1.53.3，精确固定 |
| async-trait | 0.1.92，由 Cargo.lock 固定 |
| openssl / openssl-sys | 0.10.82 / 0.9.118 |
| openssl-src | 300.6.1+3.6.3 |
| 测试进程实际 TLS 库 | OpenSSL 3.6.3 9 Jun 2026 |
| 系统 openssl 命令 | OpenSSL 3.5.7；不是测试所用库 |

通过 cargo metadata --locked 检查锁定依赖的 rust_version 声明，没有发现声明要求高于 1.95 的包；这不是最低工具链实测。完整传递依赖版本见 Cargo.lock。

Pingora 的 OpenSSL 适配层无条件开启 vendored。`cargo tree --locked -e features -i openssl-sys` 显示该特性来自 pingora-openssl；`ldd` 测试二进制无 libssl/libcrypto 动态依赖；`cargo test --locked --test feasibility pingora_listener -- --nocapture` 输出实际 OpenSSL 3.6.3。因此构建基线采用上游默认内置源码方案，没有修改第三方源码或冒称使用系统 3.5.7。系统库替代方案尚未验证。

## 2. 可复现命令与观察结果

| 命令 | 实际结果 | 判定 |
| --- | --- | --- |
| cargo fmt --check | 无格式差异 | 通过 |
| cargo check --locked | 完成 dev 检查 | 通过 |
| cargo clippy --locked --all-targets -- -D warnings | 完成，无警告 | 通过 |
| cargo test --locked | 初始化入口 0 测试；集成验证 7 通过、0 失败 | 通过 |
| cargo test --locked --test feasibility | 7 通过、0 失败 | 通过 |
| cargo test --locked --test feasibility pingora_listener -- --nocapture | 输出 TLS library: OpenSSL 3.6.3 9 Jun 2026 | 通过 |

测试源码即可复现实验步骤，不依赖仓库中的 PEM、密钥或额外测试框架。证书与 RSA 私钥只在内存中生成，临时 UDS 目录为 0700，Socket 为 0600，后端仅绑定 loopback 随机端口。

## 3. 测试行为与证据位置

| 测试 | 实际观察 | 对应目标 |
| --- | --- | --- |
| configuration_contract_examples_parse_as_kdl_v2 | 契约中的 4 个合法 KDL 块可解析；原文 17 个块迁移布尔字面量后均可解析 | 001-AC01 |
| v2_rejects_legacy_bare_booleans | spa true 被拒绝，spa #true 产生真正布尔值 | 001-AC01 |
| pingora_listener_exposes_the_same_callbacks_and_http2_settings | TlsSettings 接受 TlsAccept 回调、H2 配置、禁用会话恢复设置 | 001-AC03 |
| new_handshakes_use_new_certificates_without_closing_existing_streams | 单 ArcSwap 从代次 1 发布至 2；新客户端验证新证书，旧 TLS 流仍可往返；旧请求 Arc 保留旧路由，新读取选择新路由；分别协商 HTTP/1.1 与 h2 ALPN | 001-AC03 |
| handshake_crossing_publication_keeps_its_original_selection | 在选证书与握手完成之间强制发布；连接仍报告旧证书与旧代次，新活动配置为代次 2，没有在完成回调重新 load 错配 | 001-AC03 |
| unknown_sni_cannot_receive_another_sites_certificate | 未知 SNI 的服务端与客户端握手都失败，没有其他站点兜底证书 | 001-AC02、001-AC03 |
| pingora_streams_before_upstream_completion_and_drains_during_shutdown | 真实 Pingora HTTP 代理在上游被门控、尚未完成时送达第一块；观察 GracefulTerminate 后释放上游第二块，客户端继续收到，服务线程正常退出 | 001-AC03 |

源码位置为 tests/feasibility/。TLS 使用 Pingora `handshake_with_callback` 和真实 OpenSSL 握手，不是 TLS mock；代理验证使用实际 http_proxy_service 与真实本地 Socket。

配置示例通过仅证明 KDL 语法，不能证明文件存在、证书覆盖或完整 DSL 语义合法；完整校验归 002/004/005。H2 只验证 ALPN 与监听设置，未发送 HTTP/2 帧；完整 HTTP/2 路由由 003/004 验收。TLS 上的字节流往返不是 WebSocket 验收，完整 WS/SSE 与管理 Reload 归 006/007。

## 4. 首轮失败与修复

| 发现 | 原因 | 修复与复验 |
| --- | --- | --- |
| 文档误以为 v2 的裸 true 是字符串 | KDL v2 将 true 保留，必须带 # | 修正文档和用例；裸值拒绝/布尔值接受均通过 |
| 测试证书信任选择失败 | 两个临时自签证书具有相同 subject | 使用不同代次主体、相同合法 DNS SAN；保留完整客户端链/主机名验证，更新后通过 |
| Tokio 1.54.1 在 Pingora UDS 转换时 panic | Pingora 使用 BufStream::with_capacity(0, 0)，Tokio 新版拒绝零读缓冲容量；源码同时存在 TCP/virtual 的同类路径 | 精确固定兼容的 Tokio 1.53.3，UDS 真实代理与关闭场景通过；升级依赖前必须重跑该场景 |
| 初始小响应的流式验证等待超时 | 测试宣称实时流却使用 Content-Length；Pingora 对该路径可能合并写出，不能以此代表无长度流 | 使用真实分块实时响应，在发送第二块前要求第一块实际到达；严格门控用例通过。生产 SSE 适配仍须在 006 结合实际头部/冲刷行为验证 |
| 以为默认连接系统 OpenSSL | pingora-openssl 无条件启用 vendored | 用 feature tree、ldd 和运行时版本核对，修正文档为实际 3.6.3 基线 |

首次编译还修正了 OpenSSL owned/ref 临时值借用不匹配。以上失败未被忽略或标为通过，均以修正后的完整检查为准。

## 5. 完成后复审

复审方式：执行者重新阅读全部新增 Rust 验证代码、配置和架构文档，逐项对照 001-T01–T04 与原文示例，再执行格式、静态检查和完整测试；未委托外部代理，不声称独立安全审计。

| 复审项 | 结论与位置 |
| --- | --- |
| 原文所有 KDL 示例都有迁移或兼容结论 | configuration.md 第 8 节逐项对照；17 块语法实验覆盖 |
| 域名、路径、root、继承、布尔与命令命名没有依赖声明顺序的含混 | configuration.md 第 1–7 节 |
| 路由与 TLS 使用单个发布根，完成回调不重新读取活动指针 | architecture.md 第 3–4 节；跨发布握手测试 |
| 新请求与旧握手代次边界明确 | 每请求固定快照，旧连接证书不追溯更换；未承诺同一连接永久旧路由 |
| 默认不信任转发头，内网后端允许范围与元数据拒绝明确 | architecture.md 第 5 节 |
| Socket 权限、身份、帧大小、期限、BUSY 与结果未知有明确定义 | architecture.md 第 7–8 节 |
| 不把原型误作生产实现 | main 未加入服务逻辑，001 的测试目录明确隔离；文档列出后续验收职责 |
| 没有新增 unsafe、恢复性输入 unwrap 或持久私钥 | 审阅 tests/feasibility/ 全部代码；证书和密钥仅内存生命周期 |
| 依赖兼容问题有约束和回归路径 | Tokio 精确固定；完整真实流式与排空场景保留 |

结论：技术机制及本阶段验证通过。用户已提交访谈 a627c1ef-9010-4a54-9bb7-c1a37a86b257，明确确认配置契约、依赖/快照/管理安全基线，并批准原始工程、需求、已批准计划与 001 交付物一起纳入首次提交。001 的任务和验收项已据实际证据勾选，阶段在独立完成提交成功后关闭。001 不自动进入 002。

## 6. 上游依据

本轮实际读取 crates.io 下载的 0.9.0 源码；在线 0.9.0 docs.rs 构建失败及部分页面 404，因此 API 判断以本地发布源码和编译/运行结果为准。

- https://crates.io/crates/pingora/0.9.0
- https://docs.rs/crate/pingora-core/0.9.0/source/src/listeners/mod.rs ：TlsAccept 与握手扩展。
- https://docs.rs/crate/pingora-core/0.9.0/source/src/listeners/tls/boringssl_openssl/mod.rs ：with_callbacks、enable_h2。
- https://docs.rs/crate/pingora-core/0.9.0/source/src/protocols/tls/boringssl_openssl/server.rs ：证书回调、握手完成回调与 SslDigest 传递。
- https://docs.rs/crate/pingora-core/0.9.0/source/src/server/mod.rs ：RunArgs、ShutdownSignalWatch、优雅关闭。
- https://docs.rs/crate/pingora-proxy/0.9.0/source/src/proxy_trait.rs ：每请求 CTX 与分块请求/响应过滤器。
- https://docs.rs/kdl/6.7.1/kdl/ ：v2、显式 v1 功能及最低 Rust 版本。

未修改任何下载的第三方源码，没有安装系统包，没有增加测试框架、mock 库或 dev-dependencies。
