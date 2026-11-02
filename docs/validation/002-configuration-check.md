# 002 配置系统、check 与复审记录

## 交付范围

以 001 已确认的契约实现配置加载、严格 KDL v2 解析、导入、Profile 规范化、全局语义校验和阶段性 check。用户在实施期间明确要求“用 clap 库做 cli 处理”，已使用 Clap 派生解析替换初始手写 CLI。

- `src/main.rs` 仅委托 CLI；`src/lib.rs` 提供共享模块入口。
- `src/cli/mod.rs` 使用 Clap 定义 check、默认配置路径、帮助、版本和参数错误；`src/cli/check.rs` 只负责调用与结果输出。
- `src/config/{source,error,loader,glob,preflight,model}` 负责来源、错误、文件加载、确定性 glob、解析前深度限制和模型。
- `src/config/parser/` 按顶层、server、site/route 和标量类型拆分严格解析；`profile.rs` 处理前向引用、循环和确定性覆盖。
- `src/config/validate/` 负责域名、路由、监听、TLS 引用、上游 origin 与字面量地址策略；不启动网络服务。
- `src/config/prepare.rs` 提供 ResourcePreparer 和 PreparedSnapshot<Resources>，build 统一串联加载、解析、规范化、校验和准备。
- `examples/config.kdl` 和 `examples/multi/` 提供实际可检查的单文件/多文件配置。

没有实现监听、生产代理、真实 TLS 证书检查、安全 root 打开或 Reload 发布。ConfigurationOnly 遇到 TLS 定义或静态 root 必须返回 CONFIG_UNSUPPORTED/退出码 1；基础代理配置成功时明确输出检查范围，不假装检查了 DNS、连通性或资源。该限制符合本阶段验收边界，完整资源准备留给 004/005，上游实际连接策略留给 006。

## 实际执行的检查

| 命令或步骤 | 结果 |
| --- | --- |
| cargo fmt --check | 通过，无格式差异 |
| cargo check --locked | 通过 |
| cargo clippy --locked --all-targets -- -D warnings | 通过，无警告 |
| cargo test --locked --test configuration | 002 的 15 项集成验证通过 |
| cargo test --locked | 15 项配置验证与 001 的 7 项机制验证全部通过，共 22 项、0 失败 |
| target/debug/rill --help / check --help / --version | 均返回 0；Clap 展示命令、默认配置路径和版本 |
| 两个 examples 入口执行 check | 均返回 0，并明确输出配置检查范围 |

Rust/Cargo 与 001 相同，使用 1.99.1；未声称在声明最低版本 1.95 实测。依赖仅增加用户明确要求的 Clap 直接声明，锁定为 4.6.7；该版本此前已在 Pingora 的传递依赖中。Cargo.lock 只增加 rill 对 clap 的依赖关系，没有升级原有依赖。未新增 dev-dependencies、测试框架、mock 库或系统工具。

## 验收证据映射

证据源码：`tests/configuration.rs`。各测试使用真实加载管线或真实 CLI 进程，配置输入内联构造，不使用替代加载器。

| 验收 | 测试与观察 |
| --- | --- |
| 002-AC01 严格解析与来源诊断 | strict_schema_rejects_unknown_nodes_types_properties_and_duplicates：未知节点、属性、注解、错误子块、错误布尔类型、重复标量、超时溢出、代理混用静态字段均拒绝；错误有文件、节点、行列且不打印 secret-sentinel 参数 |
| 002-AC01 语法与 CLI 退出码 | check_reports_syntax_locations_and_cli_argument_exit_codes：裸 true 定位至第三行并返回 1；缺命令、缺参数、未知参数以及尚未提供的 run/reload 返回 2 |
| 002-AC02 导入与来源路径 | nested_globs_load_in_sorted_order_and_keep_defining_file_paths：嵌套相对导入及字符类 glob 按 a1、b2 顺序加载，root 基于定义文件而非入口文件 |
| 002-AC02 循环与别名 | import_cycles_and_file_aliases_are_rejected_with_both_locations：循环返回 IMPORT_CYCLE，硬链接重复返回 DUPLICATE，保留两处来源；对应原文 T13 |
| 002-AC02 继承优先级 | local_values_override_ordered_profiles_even_when_written_before_use：后引用 Profile 覆盖前一个，本地值即使写在 use 前也最高；route Profile 覆盖 site 默认 |
| 002-AC02 更新后重新构建 | profile_changes_rebuild_effective_route_parameters：将导入 Profile 的 read-timeout 从 4s 改为 8s，重新 build 后得到新值，保留定义和引用来源；覆盖 T12 的配置部分，不声称完成运行时 Reload |
| 002-AC02 无效引用 | all_profile_definitions_are_checked_for_cycles_and_unknown_references：未使用的 Profile 也会检查循环和未知引用，不静默忽略 |
| 002-AC03 全局冲突 | conflicting_domains_routes_and_missing_tls_references_are_rejected：大小写/末尾点域名冲突、简写与显式兜底冲突、百分号大小写模式冲突及缺失证书引用均失败；冲突保留相关位置，对应 T14 |
| 002-AC03 上游策略 | upstream_origins_cannot_bypass_literal_target_policy：拒绝非 HTTP/HTTPS、凭据、base path、query、fragment、零端口、元数据地址、映射 IPv6 绕过、传统数字/十六进制 IPv4 写法及其尾点形式；显式 CIDR 可收窄 loopback 访问 |
| 002-AC03 资源边界 | missing_resource_preparers_cannot_report_success：TLS/root 配置不会因只检查语法而返回成功，stdout 为空，stderr 为 CONFIG_UNSUPPORTED，退出码 1 |
| 002-AC03 HTTP/TLS 混合 | http_only_sites_can_share_a_server_with_tls_sites：纯 HTTP 站点和有 TLS 引用的站点可共存；仍不会绕过未实现的证书准备 |
| 002-AC03 稳定入口 | entry_symlink_remains_the_reload_entry_after_resource_preparation：准备结果保留原入口路径，更新入口符号链接后重新 build 能读取新站点，而不是永久读取旧目标 |
| 002-AC03 有界解析 | excessive_kdl_nesting_is_rejected_before_the_recursive_parser：1000 层结构、块注释、节点/参数 slashdash 注释及 Unicode 行分隔符后的深嵌套均报 CONFIG_LIMIT；32 层可交给解析器正常拒绝未知 DSL，字符串里的大量大括号不误计数 |
| 002-AC04 示例与无副作用 | check_accepts_single_and_multi_file_examples：两个入口成功；check_does_not_bind_listeners_or_contact_the_control_socket：声明端口已被其他监听占用时 check 仍成功，管理 Socket 无连接，配置内容未改变 |

## 额外手动检查

使用临时目录和真实 `target/debug/rill check --config PATH` 逐项执行，均符合预期：

- 不存在的 glob 返回 RESOURCE_IO/1；未知节点返回 CONFIG_UNKNOWN/1，错误不含参数中的 secret-value。
- Unicode 行分隔符后的未知节点正确报告第三行。
- `upstream-allow "::ffff:127.0.0.0/104"` 与对应映射地址可按 IPv4 策略通过。
- 合法的 `HTTP://0xapi.example.test:3000` 能通过 origin 语义校验，不因 DNS 标签恰好以 0x 开头被误拒绝；没有宣称该测试域名可解析。
- 1000 层嵌套返回 CONFIG_LIMIT/1，不再异常中止；尚未支持的 snippet 返回 CONFIG_UNKNOWN/1。
- 原始字符串、多行字符串及原始多行字符串内各放入 1000 个大括号，均进入正常语义拒绝而非触发结构深度限制，证明扫描器没有将字符串内容当作结构。

## 完成后复审与修正

复审由执行者重新阅读配置/CLI 实现，对照 001 契约和 002-T01–T04，执行边界输入、真实 CLI、静态检查及全量测试；不是外部独立审计。

| 发现 | 修正与验证 |
| --- | --- |
| 原始 KDL 解析器处理 1000 层结构或连续 slashdash 注释会栈溢出，真实 CLI 退出 -6 | 添加解析前有界扫描，正确跳过普通/原始/多行字符串和注释，将结构、块注释和连续 slashdash 注释限制为 32 层；在同一最小回归中覆盖节点/参数注释递归，并验证 Unicode 换行不会绕过扫描 |
| 若保存 canonicalize 后的入口，未来 Reload 会固化到旧的符号链接目标 | 保留原始绝对入口，实际来源仍记录解析后的文件路径；入口链接更新回归通过 |
| 初版将任何 HTTPS 监听误当作所有站点必须 TLS | 按契约允许纯 HTTP 站点共存，仅要求 HTTPS 至少有一个 TLS 站点、全局重定向站点有证书引用；集成回归通过 |
| 过宽的 0x 检查可能误拒绝正常 DNS，传统数字地址尾点又可能逃出检测 | 先规范化域名，再只拒绝整串符合数字地址形式的名称；负向回归与合法 DNS 手动检查通过 |
| 映射 IPv6 CIDR 与 IPv4 地址族可能不一致 | 对映射 CIDR 规范化地址及 prefix；短于 96 的映射前缀拒绝；正向手动检查和目标拒绝回归通过 |
| 语法错误原因过于笼统、未知节点不显示名称 | 使用 KDL 首条诊断原因及实际节点名，保留文件/行列；节点名和文件路径转义展示，不输出参数或完整源码 |

还复核了文件数量/字节限制、确定性 glob、循环检测、局部显式值优先、路径定义来源、单一构建入口、类型与未知字段严格拒绝，以及 check 不产生监听或发布副作用。没有新增 unsafe，没有以 unwrap 处理可恢复输入。

## 交接

002 的配置与 CLI 交付已验收，所有任务和验收复选框可依据上述实测更新。阶段在独立完成提交成功后关闭；提交失败仍待提交，不自动进入 003。G01 仍需 004/005 的完整资源校验，不提前勾选总目标。
