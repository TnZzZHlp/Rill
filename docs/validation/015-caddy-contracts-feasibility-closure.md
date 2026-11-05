# 001 契约与技术可行性收口

## 状态与证据边界

实施基线 `e98db9f`，工具链 rustc/cargo 1.99.1，未验证 MSRV 1.95。001 只交付精确契约与隔离原型，不把 002–010 的生产功能提前实现。

本轮补齐正则捕获 EOF 回退、同进程池政策隔离、DAV/日志跨快照资源生命周期。技术任务 001-T01–T03 已具备证据，独立复审通过；在提交最终 CONTRACT.md 与本记录供验收后，用户明确回复“验收通过”，001-T04 与 001-AC02 已通过。001 完成提交以包含本记录的独立提交为准，不自动进入 002。

主工程测试在临时可访问工作区以 UID 65534 执行：190 passed、0 failed、1 ignored；唯一忽略项需要隔离 DNS fixture。主 feasibility 31/31；容器协议和连接池矩阵连续三轮，每轮 50 项判定通过，共 150 项；独立工具的 registry 测试 1/1。fmt、check、clippy、release 构建与 doc-test 均通过。

本轮没有修改 src/ 生产代码，也未更换框架、操作生产 Caddy、下载依赖或镜像。主 Cargo.lock 仅在 rill 依赖列表增加原已锁定的 regex/regex-syntax 名称，没有新包或版本变化。

## 1. 正则与分块无关性

位置：`tests/feasibility/regex.rs`。

- `regex = 1.13.1`、`regex-syntax = 0.8.11` 精确提升为测试直接依赖；许可证均 MIT OR Apache-2.0，默认 feature，来源为现有本地 Cargo registry。
- HIR maximum_len=None 表示最坏待决可能无界。原型不使用固定重叠窗口、不假定输入块边界等于匹配边界；输入逐块写入固定 Box<[u8]> 待决区域，1 MiB 预算内允许直到 EOF，超限失败。
- EOF 对完整待决序列执行 Rust regex 左最先捕获；$1–$9 与 $$ 展开到消费 sink，每片最多 64 KiB，输出累计预算前置检查。测试收集结果的 Vec 属于断言消费者，不是变换器额外聚合缓冲。
- 8 类无界模式、216 种二分点及固定步长分块，与 regex 的整段 replace_all 逐字节一致：贪婪/非贪婪、分支、可空匹配、未匹配捕获、Unicode 字符跨块、行锚和二进制输入。
- 非均匀 600 KiB + 400 KiB + 24 KiB 正好达到 1 MiB，仍通过；额外字节导致永久 Pending 失败；捕获扩张越过输出预算失败，不返回截断成功。

数学边界：任意分块只进行相同字节的有序拼接，未输出不确定前缀；EOF 上的输入、同一 regex 和替换函数完全相同，因此成功结果与分块无关。有限预算导致部分输入明确失败是 D06 已接受的语义，不承诺任意正文必然成功。字面量提前输出由既有两个用例验证；007 负责生产适配、全局额度、编码与计时器落地。

原始输出见 [feasibility.log](data/015-contracts-feasibility/feasibility.log)：

```text
unbounded regex: 8 patterns, 216 partitions match batch output
unbounded regex: exact 1MiB pending accepted; pending/expanded output overflow rejected
```

复审发现过 Vec 按容量倍增可能超过声明预算的问题，已改为固定 Box 待决区域；复审关闭该问题。CPU 预算精确限定为完成预算与执行等待预算，不能假称中断单次不可抢占 Rust 库调用；超时后仍运行任务继续占用许可，迟到结果不得发布。该数值和取舍在最终用户验收清单中单独确认。

## 2. gRPC 与同进程连接池隔离

位置：`tests/feasibility/grpc_container/`。原有四条 h2c/TLS h2 直连和代理路径 × 9 检查、两项 TLS 负面共 38 项保持通过；扩展同一 Pingora proxy 实例中的政策选择，以及真实 HTTP/1 对照后端。

同进程验证：

1. good CA + h2 在同一 TLS endpoint 建池，两次调用复用同一 fd。
2. 同目标同验证名但错误 CA 必须握手失败；同目标但错误验证名也必须失败，不能复用已建立的 good 连接。
3. 回到 good 政策仍复用原连接；两个 h2c 目标分别返回后端 ID one/two，回到第一个目标仍复用其连接。
4. 先 h2 后 h1 时不复用已有 h2；h1 新握手被仅支持 h2 的 TLS 后端拒绝。反方向先建立真实可复用 h1，再对同地址尝试 h2；后端确实接受新的 h2 preface 连接，回 h1 仍复用原连接。
5. 原型按完整协议、SocketAddr、TLS、验证名与 CA 字节精确比较政策，由 checked_add 分配唯一 u64 token，显式写入 group_key。不能假定默认 PeerOptions 自动隔离 CA/ALPN：Pingora 默认 HttpPeer hash 未包含这两项。
6. 强制让错误 CA 政策使用 good 的 group_key，模拟池哈希键碰撞。connected_to_upstream 在写业务 HTTP 前比较独立政策 token，记录 `POOL_IDENTITY_MISMATCH` 并拒绝；后续 good 仍成功。group_key 不是授权身份。

原型有限 fd→token 表用于验证框架挂钩，不直接作为生产连接登记实现。契约已要求 005 的登记绑定连接出生身份、关闭回收及有限额度，不能以裸 fd 当永久身份或让全局哈希值代替精确政策核对。

三轮完整原始输出：[grpc-1.log](data/015-contracts-feasibility/grpc-1.log)、[grpc-2.log](data/015-contracts-feasibility/grpc-2.log)、[grpc-3.log](data/015-contracts-feasibility/grpc-3.log)。首轮完整池日志：[pool.log](data/015-contracts-feasibility/pool.log)。每轮 50 PASS，清理结果均 passed=true、cleanup=true、cleanup_errors=[]。

```text
POOL policy=good reused=false fd=15 token=1
POOL policy=good reused=true fd=15 token=1
POOL policy=target-one reused=false fd=16 token=5
POOL policy=target-two reused=false fd=17 token=6
POOL policy=target-one reused=true fd=16 token=5
POOL_IDENTITY_MISMATCH policy=forced-collision reused=true fd=15 expected=2 actual=Some(1)
POOL policy=plain-h1 reused=false fd=18 token=7
POOL policy=plain-h1 reused=true fd=18 token=7
POOL policy=plain-h2 reused=false fd=19 token=8
```

此前把通用 HTTP 请求 trailers 列为必需缺口属于助手扩大范围：gRPC 四类调用的请求用 EOS 结束，原需求涉及的是响应 grpc-status/grpc-message/trailers-only。Pingora 公开请求读写路径未暴露请求 trailers 转发接口，当前不宣称此通用能力；不是把响应 trailers 降级。h2c/TLS h2 四类、响应 trailers、客户端取消、deadline 与兄弟 RPC 均见 014 及本轮矩阵，生产配置/Reload 属后续阶段。

## 3. DAV 稳定身份、写事务与锁

位置：`tests/feasibility/write.rs`，真实临时目录、openat2/renameat、线程及 ArcSwap，不替换为 mock。

- 同目录临时写入和原子 rename；已经打开的旧 FD 继续读 old，新 open 读 new；成功没有临时残留。
- 临时 PUT 在取消或实际 rename 失败后清理临时条目，旧目标保持原文。
- 已发生的父目录搬移并替换为越界 symlink，或相同路径换成不同 inode，复验拒绝写入；外部目标不被修改。
- registry 的 dev/ino 和逻辑锁独立于快照。ArcSwap 真正发布新 snapshot 后共享同一 registry；错误 token 不能解锁；候选 root 身份变更返回 RESTART_REQUIRED，旧 snapshot/锁不改变；容量、冲突、TTL 与重启失锁有独立边界断言。
- namespace guard 跨旧/新 snapshot 共享。真实 MOVE 线程先 try_lock 证明被 PUT 的 guard 阻挡；PUT 完成后释放 guard，MOVE 才执行，结果文件内容完整。

保证针对遵守同一 namespace 锁的内部 DAV 操作；与现有架构一致，管理员/服务 UID/root 在本地文件系统的外部并发变更是受信边界，不声称消除了恶意管理员在重验后的 TOCTOU。锁不能替代 root 路径约束，也不能替代 WebDAV token 鉴权。持久属性和方法互通由 008 实现，不把本轮机制测试冒充完整 WebDAV 服务。

## 4. 日志无副作用准备与跨代 worker

位置：`tests/feasibility/logging.rs`。

- prepare 不创建文件；真实文件 worker 通过 mpsc 收消息并在写入/flush 完成后回执。
- 新快照发布时复用同一 Arc<Worker> 与原文件 inode；没有重新打开或轮转活动文件。
- 两个新文件激活后第三个目标（目录）实际失败，回滚删除前两者；ArcSwap 保持原 snapshot，原活动文件的 inode 与内容不变。
- 发布替代 sink 后，持有旧 snapshot 的请求仍可向旧 worker 写入；在最后旧 owner 释放之前关闭信号不可见，释放后实际 worker 退出并关闭文件。

候选 RAII 的生产接口已在契约闭合为 prepare→activate→store→无失败 commit：所有可失败步骤在 store 前完成，回滚只移除候选自己拥有的新文件，绝不截断/rename 活动文件。多文件系统路径不承诺进程崩溃原子回滚；单快照发布仍是唯一政策切换点。009 负责完整轮转、磁盘/队列限制、worker 取消与回收，不将测试线程的同步 join 写进业务请求 Drop。

## 5. 契约复审修订

契约：[`CONTRACT.md`](../plans/caddy-feature-parity/CONTRACT.md)。两条独立审阅通道分别检查算法/资源原型和精确接口闭合，发现并修复：

- matcher 只读 context 无法消耗实际短路步数或输出捕获：改为受限可变 MatchContext + MatchResult；明确当前 URI、false/not/all/any 捕获规则。
- 响应过滤只保存静态 ID 无法冻结请求级捕获：改为 ResponseFilterInstance，Arc 捕获共享、36 KiB 字节预算与 256 实例/64 KiB 元数据预算。
- DAV activate 可失败但发布先后未闭合：改为可回滚 ActivatedStorage + 不可失败 commit。
- 既存日志与硬磁盘上限矛盾：准备/激活有界扫描检查，不合规拒绝候选，不在失败候选中整理旧文件；声明每条旧请求的轮转政策和绝对硬限额。
- try-files 全 Missing 与 strip-prefix 不命中：明确本地可捕获 404 和错误码，保留查询，不隐式继续原路径。
- 正则 Vec 容量增长超限：固定 Box 区域。

其余精确化包括统一 CONFIG_UNKNOWN、Content-Encoding 保护、无 trusted-proxy 时使用真实 peer IP、绝对 FD/全局编译缓存/池身份预算。已将上述精确化及算法/资源边界提交最终验收；用户随后明确回复“验收通过”，确认接受最终契约和本记录，而非仅以先前“整体完成”实施指令推断验收。

## 6. 质量门禁与复现

```sh
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo test --locked --offline --test feasibility -- --nocapture --test-threads=2
cargo test --locked --offline --doc -q
cargo build --release --locked --offline -q
cargo fmt --manifest-path tests/feasibility/grpc_container/Cargo.toml --check
CARGO_TARGET_DIR="$(readlink -f target)" cargo clippy --locked --offline \
    --manifest-path tests/feasibility/grpc_container/Cargo.toml --all-targets -q -- -D warnings
CARGO_TARGET_DIR="$(readlink -f target)" cargo test --locked --offline \
    --manifest-path tests/feasibility/grpc_container/Cargo.toml -q
python3 tests/feasibility/grpc_container/run.py
```

全部通过。主测试权限用例不能在 root-only 路径盲跑：复制当前输入文件到独立 0755 临时工作区 `/var/tmp/rill001-closure-suite-bGidmR`，其自身 target 为普通目录，不触碰仓库共享 target；root 离线 no-run 编译，Cargo JSON 选出 profile.test=true 的全部八个工件，再以 nobody 执行每个测试二进制 `--test-threads=2`。原始输出：[suite.log](data/015-contracts-feasibility/suite.log)。

```text
rill lib: 36 passed
rill main: 0 passed
configuration: 27 passed
feasibility: 31 passed
http_proxy: 21 passed
https: 20 passed
reload: 28 passed; 1 ignored
static_files: 27 passed
```

总计 190 passed、0 failed、1 ignored；忽略项依赖独立 DNS fixture，不属于本阶段验收，不启用。没有声称 include-ignored 全套通过。

容器均使用本地 Debian 镜像 ID `394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853`，仅 loopback 发布，私有 bridge、非特权 UID、资源限额与清理机制沿用 014。结束后没有本轮容器/网络/代理，临时私钥已删除；日志仅包含隔离地址、路径和原型状态。
