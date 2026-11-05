# Caddy 能力补齐 002：规则与响应验证记录

日期：2026-11-05。此记录对应 Caddy 能力补齐的 002，不是历史 Rill 计划的 002。用户批准的独立 PikeVM 运行工作区额度见契约 5.1；以下保留实施期间的检查及失败记录；最终独立复审与非 root 全套收口结果见末节。

## 实际检查

- `cargo fmt --all --check`：通过。
- `cargo check --locked --offline -q`：通过。
- `cargo clippy --locked --offline --all-targets -q -- -D warnings`：通过。
- `cargo test --locked --offline -q --test configuration -- --test-threads=2`：29 通过。
- `cargo test --locked --offline -q --test http_proxy -- --test-threads=2`：23 通过。链测试通过 loopback 验证 branch 无命中 404、命中 continue、glob/regex、固定 http1 代理 Host、静态自定义 index、HEAD、204、304、重复请求头及可信和不可信转发地址。
- `cargo test --locked --offline -q --test reload chain_failed_reload_and_keepalive_generation`：1 通过。非法正则候选返回 `POLICY_INVALID_REGEX`，旧响应保留；成功 reload 后同一 keepalive 连接的下一请求取得新代响应。
- `cargo test --locked --offline -q --test reload chain_proxy_stream_keeps_old_snapshot_across_reload`：1 通过。旧链代理 SSE 在新规则发布后继续发送原流，新请求得到新响应。
- `cargo test --locked --offline -q --lib http_policy::workspace::tests::workspace_exhaustion_releases_capacity`：1 通过；预留耗尽后释放可再次预留。
- `cargo test --locked --offline -q --test https -- --test-threads=2`：20 通过，验证既有 HTTPS/SNI 回归。
- `cargo test --locked --offline -q --test reload -- --test-threads=2`：28 通过、1 忽略、1 失败；失败为 root 下的 `watch_permission_denied_retries_are_bounded_and_chmod_recovers`，明确要求非特权 UID，须在可访问的非 root 环境复测。忽略项要求隔离 DNS fixture，未启用。
- `cargo build --release --locked --offline -q`：通过。
- `git diff --check`：通过。

## 边界

错误链及 `error-status` 运行适配归后续阶段；header 动作、rewrite、认证、HTTP/2 上游等后续功能未实现。旧代流测试只覆盖单个 SSE 场景，仍需独立复审及非 root 全套验证。PikeVM 工作区按 NFA 状态、捕获槽和 union 出边上界预留；库 `memory_usage` 不包含 epsilon 栈 capacity，不作为完整工作区测量。编译策略内存按持有对象容量和库报告的 NFA 堆大小记账，不等同 allocator RSS。

## 复审修复和工作区上界核对

后续修复在 PikeVM `build` 前按每程序 2 MiB、元数据上界、包装和索引额度预留进程编译额度；成功后根据 NFA `memory_usage` 缩减，失败时释放，候选 8 MiB 使用最终记账而非 2 MiB 临时值。库的 `nfa_size_limit` 是近似值，不覆盖短命 AST、HIR、compiler builder 及辅助 heap；最终 NFA 报告超过 2 MiB 时拒绝。短命编译工作区未计入持久策略额度，不能将此限额等同 RSS。

以下核对依据本机 registry 的 `regex-automata-0.4.18` 源码及 rustc 1.99.1 的 `alloc/src/raw_vec/mod.rs`，不是对其他版本或 allocator RSS 的承诺。设 S 为 NFA 状态数、G 为捕获槽数、E 为所有 Union 的分支数与 BinaryUnion 的两条分支数之和：

- `nfa/thompson/pikevm.rs` 的 `Cache::new`、`ActiveStates::reset`、`SlotTable::reset` 创建两份槽表，每份逻辑长度为 `S*G + max(G,2)`，元素 `Option<NonMaxUsize>` 在本机为 8 字节；`util/sparse_set.rs` 的 `SparseSet::resize` 每份 active set 创建两个 S 长度的 StateID 数组，StateID 为 4 字节。当前公式对上述数组分别乘 2 以留增长余量。
- `epsilon_closure` 初始压栈一次；`epsilon_closure_explore` 在对某状态执行动作前以 `next.set.insert` 去重。重复目标仍可能在入栈时并存，不能说每个入栈帧都唯一：每个实际展开的 Union 至多推入其分支数减一，BinaryUnion 推入一条，Capture 至多推入一个恢复帧，因此一次闭包的累计推入量及同时存在帧数均不超过 `1 + E + S`。
- rustc 1.99.1 的 `RawVecInner::grow_amortized` 选择 `max(2*旧容量, 所需容量, 最小非零容量)`；该尺寸范围的最小非零容量为 4。增长时旧、新分配可能暂时共存；栈实际帧 16 字节时，旧、新容量合计可由 `3*(1+E+S)*16` 加小容量余量覆盖，当前工作区公式为 `2*(1+E+S)*32` 字节，另加 4096 字节余量。此 16 字节通过公开 `StateID`、`SmallIndex`、`Option<NonMaxUsize>` 在本机构造同字段影子 enum 测得，**不能单凭该影子类型证明库私有 enum 的布局**；32 字节上界及编译器布局假设仍需独立复审。
- 当前 `execute.rs` 调用 `search_slots`，最多传入栈上的 20 个槽，不调用 `create_captures`；单模式 `search_slots` 在空匹配特例也使用两个栈槽，不产生多模式分支的额外堆向量。`Cache::memory_usage` 使用 stack 的 len 而不是 capacity，故不能用于证明该界。预留在 `create_cache` 前取得，cache 先于 permit 释放。

依赖信息取自本机 registry 各版本 `Cargo.toml`：`regex-automata =0.4.18` 默认 features（含 std、syntax、perf、unicode、meta、nfa、dfa、hybrid），许可证 MIT OR Apache-2.0，生产代码仅显式使用 PikeVM；`regex-syntax =0.8.11` 默认 std、unicode，许可证 MIT OR Apache-2.0；`http =1.5.0` 默认 std，许可证 MIT OR Apache-2.0。后两项从既有 dev 依赖提升为 direct；`regex-automata` 已锁定，`Cargo.lock` 的 rill 依赖列表只增加此名称，没有新增包或版本变化。启用库 feature 不代表运行时创建 DFA。

父代理报告的非 root 全套初次运行位于 `/var/tmp/rill002-suite-8hQjkd`，有 8 个结果工件；其中 3 个既有 reload 测试首次出现 ENOENT，立即单独重跑全 reload 后 30 通过、1 忽略。父代理初步怀疑既有 fixture 只等待 HTTP 而未等待 control socket 的启动竞态，尚未确定根因，不能称为产品或权限问题；这是本轮修复之前的结果，修复后仍需重新运行。

本次复审修复后重新运行：`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings` 均通过；`cargo test --locked --offline -q --test configuration -- --test-threads=2` 为 30 通过；`cargo test --locked --offline -q --lib regex_build_admission_uses_final_candidate_size` 为 1 通过；`cargo test --locked --offline -q --test http_proxy chain_ -- --test-threads=2` 为 3 通过；`cargo test --locked --offline -q --test reload chain_ -- --test-threads=2` 为 2 通过；`git diff --check` 通过。新增的配置回归覆盖非法首状态参数、第二位置参数、1024 步恰好成功及少一步失败、any 短路和 branch 首个分支排他。本次未重新执行 release 构建或非 root 全套。

## 最终 NFA 元数据预留补充

对锁定的 regex-automata 0.4.18，`nfa/thompson/builder.rs` 的 `Builder::build` 将中间状态转换为最终状态，不增加状态数量，Sparse/Union 的堆载荷转换为等量 boxed 切片，构造转换不产生 Dense；builder 的 `memory_usage` 按中间状态数量和载荷长度计量。库自身测试 `state_has_small_size` 声明 64 位 builder State 为 32 字节、32 位为 16 字节；本机 64 位公开 NFA State 为 24 字节。运行前检查公开 State 不超过对应的 `4 * size_of::<usize>()`，以保留最终状态与载荷长度不超过 2 MiB 中间态预算的约束。此推导依赖该锁定库版本及其测试所描述的布局，非其他版本或 allocator RSS 的保证。

最终 `NFA::memory_usage` 还计入 Inner、单模式 start StateID 和 `GroupInfo`。`util/captures.rs` 的 `GroupInfo::memory_usage` 包含固定头及捕获索引；匿名组按 16 字节，命名组按 `2*名称字节数+36` 计，单模式捕获组数最多为模式字节数加一，捕获名称总字节数不超过模式字节数。故在编译前额外以 checked 算术预留 `64*(pattern.len()+1)+4096` 字节，覆盖组信息及固定头；生成 glob 也使用生成后的模式长度，尽管它不生成捕获组。该额外预留仅用于进程编译准入，最终 NFA 超过 2 MiB 仍拒绝，成功后仅保留实际记账，不改变候选 8 MiB 或进程 64 MiB 编译策略额度。短命 AST/HIR、builder 及 allocator 元数据仍不计入该持久策略记账。

父代理报告后续已重新运行非 root 全套，8 个结果工件均通过；该次运行早于本节元数据预留修改，修改后仍需重跑。

元数据预留修改后实现子代理执行：`cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo test --locked --offline -q --lib regex_build_admission_uses_final_candidate_size`、`cargo test --locked --offline -q --test configuration chain_configuration_compiles_and_rejects_invalid_candidates` 和 `git diff --check`，均通过。

## 最终复审与收口

独立 reviewer 首轮阻止提交：编译程序分配前未预留额度，以及 `respond "invalid" 200` 可绕过参数检查。修复后进一步补齐最终 NFA 元数据的临时预留；最终复审确认 must-fix 全部关闭，结论为 `OK with notes`。PikeVM 工作区另经逐项源码核对，通过意见限于锁版单模式、未启用 instrumentation 的 64 位环境；锁版、feature、模式数量或工具链布局变化必须重新核对，不将其宣称为 allocator RSS 精确上限。

最后一次生产代码修改之后，父代理重新执行并通过：

```sh
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo build --release --locked --offline -q
cargo test --locked --offline --doc -q
git diff --check
```

实际工具链为 rustc 1.99.1、cargo 1.99.1；没有验证 MSRV 1.95。`file target/release/rill` 确认为 x86-64 动态链接 ELF，不声称全静态。

非特权全套在独立 0755 工作区 `/var/tmp/rill002-suite-8hQjkd` 中执行：复制本轮源码、配置和测试输入，以 root 离线 `cargo test --locked --offline --no-run --message-format=json` 构建；Cargo JSON 选择 `profile.test=true` 的八个可执行工件，逐个 `runuser -u nobody -- <executable> --test-threads=2`。复用 `/var/tmp/rill001-closure-suite-bGidmR/target` 的可访问构建目录，不修改仓库共享 target 符号链接。最终合计 **199 passed、0 failed、1 ignored**；忽略项仍是依赖 `127.0.0.1:53` 专用解析器的历史 DNS 测试，未启用。

| 工件 | 通过 | 忽略 |
| --- | ---: | ---: |
| lib | 38 | 0 |
| main | 0 | 0 |
| configuration | 30 | 0 |
| http_proxy | 23 | 0 |
| https | 20 | 0 |
| reload | 30 | 1 |
| static_files | 27 | 0 |
| feasibility | 31 | 0 |

原始输出：[最终全套](data/016-rules-responses/suite.log)、[首轮全套失败记录](data/016-rules-responses/initial-suite.log)、[首轮失败后完整 Reload 重跑](data/016-rules-responses/reload-rerun.log)。首次 ENOENT 未定因，不将重跑成功改写为首次通过；之后两轮非 root 全套均无失败，其中最终一轮在元数据预留修改之后执行。

002-T01–T04、AC01–AC03 验收通过：统一配置管线、顺序及短路、捕获隔离、分支排他、显式继续、固定响应、可信地址、旧精确/最长前缀、Host/SNI、失败候选和跨代流均有实际检查。新增 `config/parser/chain.rs` 负责严格语法；`http_policy/model.rs`、`compile.rs`、`execute.rs`、`response.rs`、`workspace.rs` 分别负责模型、编译记账、求值控制、响应分帧和运行额度，入口保持适配职责。固定 http1 代理和静态终止动作复用原有准备资源；不操作生产 Caddy，不联网获取依赖，不恢复观测导出，不启动 003。

错误链和 `error-status` 的运行上下文属于 003；当前请求链拒绝这些节点，未宣称该后续能力已交付。主计划 G01 仍需 003，其他总目标不因本阶段完成而勾选。
