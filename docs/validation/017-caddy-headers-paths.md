# 017：Caddy 能力补齐头部、路径与错误处理验证

本记录保留 003 各实施切片与复审修复的验证历史；最终父会话复测见末节。用户已明确同意上游 authority 缺值时安全失败；003 实施、独立复审与隔离验收完成，完成提交以包含本记录的 Git 提交为准。测试在隔离 loopback 与临时目录执行，未操作生产服务。工具链为 rustc 1.99.1、cargo 1.99.1；未验证 1.95。早期切片以 UID 0 运行，末节完整套件以非特权身份运行。

## 实际执行

- `cargo fmt --all --check`：通过。
- `cargo check --locked --offline -q`：通过。
- `cargo clippy --locked --offline --all-targets -q -- -D warnings`：通过。
- `cargo test --locked --offline --test configuration -- --test-threads=2`：32 项通过，包括路径模板、非法目标、未知变量、候选和不存在 root 的配置检查。
- `cargo test --locked --offline --test http_proxy chain_ -- --test-threads=2`：7 项通过，包含三种终止响应头策略、捕获冻结、可信地址、rewrite 重入、前缀边界及受限候选。
- `cargo test --locked --offline --test http_proxy chain_paths_rewrite_strip_and_constrained_candidates -- --nocapture`：通过；覆盖原查询保留、目录/FIFO/非目录父项/越界链接不回退、编码斜线和点段拒绝、百分号单次解码。
- `cargo test --locked --offline --test static_files metadata_methods_and_conditional_requests -- --test-threads=2`：1 项通过。
- `cargo test --locked --offline --test reload failed_try_files_root_reload_preserves_old_snapshot -- --test-threads=2`：1 项通过；候选 root 准备失败后旧快照继续服务。
- `cargo build --release --locked --offline -q`：通过。
- `git diff --check`：通过。

含 `try-files` 的链（包括分支内声明）保守标记为文件系统链：先获取现有全进程 128 静态 IO 槽，再进入 `spawn_blocking`。等待和结果受入口头部期限约束；取消或超时不抢占已运行的 Linux 文件系统调用，该任务继续持有槽和请求快照直至结束，迟到结果不发布。随后真正静态服务单独取得槽，不嵌套持有。

`{upstream-authority}` 的已确认表示是验证后的连接目标 `host:port`，始终带端口、IPv6 加方括号；不是 DNS 候选地址，也不是 `proxy host` 覆盖值。实施当时没有确定上游时的行为尚待确认，暂以缺值安全失败；用户后续明确回复“同意”，最终确认见末节。

## 003-T03 错误链切片

本轮按最终代码重新运行 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings` 及 `cargo build --release --locked --offline -q`，均通过。`cargo test --locked --offline --test configuration -- --test-threads=2` 33 项通过；`cargo test --locked --offline --test http_proxy chain_ -- --test-threads=2` 9 项通过。另运行下列定向测试，各 1 项通过：

- `cargo test --locked --offline --test static_files metadata_methods_and_conditional_requests -- --test-threads=2`
- `cargo test --locked --offline --test static_files truncated_stream_never_appends_an_error_response -- --test-threads=2`
- `cargo test --locked --offline --test https dns_upstream_verifies_name_and_preserves_host -- --test-threads=2`
- `cargo test --locked --offline --test reload chain_failed_reload_and_keepalive_generation -- --test-threads=2`
- `cargo test --locked --offline --test reload failed_try_files_root_reload_preserves_old_snapshot -- --test-threads=2`

真实 HTTP 检查本地 404/405、错误页原状态与 Allow、HEAD、条件/Range 旁路、路径拒绝、上游 404 不捕获、错误链代理 200 拒绝、错误页再次缺失不递归、共享步骤及过滤器上限。`error-status` 只在错误链接受 400–599；语法接受的 401/403/502 不意味着这类来源可进入错误链。错误链代理响应 2xx/3xx 在头提交前拒绝并返回 503；4xx/5xx 正常保留，上游传输失败不递归，原 503 只能保持 503。响应声明按主链实际登记顺序，然后追加错误链实际登记顺序；条件使用最终响应的原始状态与 MIME。当前没有新增全局/site/local 声明层。

未执行完整测试套件、非特权 UID 权限拒绝测试或独立复审；不得据此勾选整个 003。

## 复审修复后定向验证

独立复审指出非法静态路径在方法检查后才解码、错误链无匹配时丢失原 405 元数据、无捕获分支清除外层捕获。修复前新增真实 HTTP 测试分别复现 405 替代 400 和分支重写得到 404；修复后：

- `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`：通过。
- `cargo test --locked --offline --test configuration -- --test-threads=2`：33 项通过。
- `cargo test --locked --offline --test http_proxy chain_ -- --test-threads=2`：11 项通过，包含非法 POST、未匹配 405 的 Allow 与过滤器、分支捕获继承及覆盖。
- `cargo test --locked --offline --test reload chain_failed_reload_and_keepalive_generation -- --test-threads=2`、`cargo test --locked --offline --test reload failed_try_files_root_reload_preserves_old_snapshot -- --test-threads=2`：各 1 项通过。
- `git diff --check`：通过。

父会话在本次修复之前执行的非 root 八个测试工件结果为 211 通过、0 失败、1 个 DNS 条件测试忽略；这是修复前阶段证据，不代表修复后的完整套件结果。后续修复后的复审和完整套件结果如下；上游 authority 缺值语义仍待用户确认。

## 修复后独立复审与父会话全套复测

独立 reviewer 逐项关闭非法路径分类、405 Allow 丢失、分支捕获三项 finding，并复核捕获、过滤器与编译额度、错误链资格、阻塞任务许可及快照生命周期。源码复审结论为 `OK with notes`；reviewer 未自行执行测试，下列结果由父会话实际运行。

在最后一次生产代码修改后，父会话执行并通过：`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo build --release --locked --offline -q`、`cargo test --locked --offline --doc -q` 和 `git diff --check`。

非 root 全套使用独立 0755 工作区 `/var/tmp/rill003-suite-NgQteZ`，复制当前源码、配置与测试输入，以 root 执行 `CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target cargo test --locked --offline --no-run --message-format=json`。按 Cargo JSON 选取 `profile.test=true` 的八个工件，逐个执行 `runuser -u nobody -- <executable> --test-threads=2`；没有修改仓库共享 target 符号链接。

| 工件 | 通过 | 忽略 |
| --- | ---: | ---: |
| lib | 40 | 0 |
| main | 0 | 0 |
| configuration | 33 | 0 |
| http_proxy | 31 | 0 |
| https | 20 | 0 |
| reload | 31 | 1 |
| static_files | 27 | 0 |
| feasibility | 31 | 0 |

合计 **213 passed、0 failed、1 ignored**。忽略项仍依赖 `127.0.0.1:53` 专用解析器，未启用。权限拒绝等已有测试在非特权身份下通过。原始输出：[suite.log](data/017-headers-paths/suite.log)。

## 用户确认与最终收口

用户对“上游尚未确定时使用 `{upstream-authority}` 明确失败，不填空、不猜测、不重放规则”明确回复“同意”。该语义已同步至契约、配置及架构文档；请求动作当时没有目标即失败，延后响应动作在代理目标确定后可使用，respond/static 没有上游则同样失败。当前适配返回 503，不生成空 authority 头。

确认后在既有三种响应路径的真实 HTTP 测试中补充请求侧和响应侧缺值场景：均返回 503，且不输出 `X-Upstream`；已有代理目标可用时仍验证规范 authority 不受 Host 覆盖影响。未修改生产实现。父会话再次执行上述 fmt/check/clippy/release/doc-test 及非 root 八工件完整套件，最终仍为 **213 passed、0 failed、1 ignored**；链接的 suite.log 为该最后一轮输出。没有启用缺少 DNS fixture 的忽略测试。

003-T01–T04、003-AC01–AC03 完成并更新计划，G01 的 001–003 责任范围验证闭合；其余总目标及后续阶段不勾选、不启动。本阶段未新增依赖、测试框架或工具，未联网、未操作生产 Caddy、未恢复观测导出，未修改既有未跟踪 AGENTS.md。
