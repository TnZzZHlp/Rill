# 018：Basic Auth 与静态令牌验证记录

本记录覆盖 004 实施、独立复审修复及最终非 root 全套检查。004-T01–T03、AC01–AC03 已验证完成；完成提交以包含本记录的 Git 提交为准。以下早期切片结果保留，最终收口见末节。

- Basic 和静态令牌在统一配置管线中准备；令牌以非阻塞 FD 检查并读取，候选跟踪 watch 依赖。主链、错误链和全部分支的令牌头均受保护。真实服务覆盖 bcrypt、Argon2id、令牌、无效/重复/缺失凭据、401 challenge、连续认证及上游凭据清除。
- 执行器按规则/分支/动作游标暂停及恢复；静态 IO 槽在等待哈希之前释放。哈希任务先获取全进程四槽许可，blocking closure 保持快照和许可；两秒超时或取消不使迟到结果生效。真实并发 cost-14 测试观察到容量拒绝、超时后仍拒绝额外任务；该测试依赖实际运行时成本。
- 用户确认 bcrypt 超过 72 字节不截断，执行同成本 dummy 验证后拒绝；Argon2 不加此限制。同一请求的同一快照、同一动作成功结果可重入复用，不同动作不复用；已用真实服务验证。固定 dummy 避免未知用户立即返回，但不同用户配置的算法和成本仍可能被计时区分，不保证隐藏用户名。
- 令牌更新经 reload 生效，错误权限候选保留旧令牌；watch 更新及旧认证流跨代完成均经真实服务测试。控制面错误保留脱敏鉴权错误码。
- `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo build --release --locked --offline -q`：通过。
- `cargo test --locked --offline --test configuration -q`：36 项通过；`cargo test --locked --offline --test http_proxy -q -- --test-threads=2`：36 项通过；`cargo test --locked --offline --test reload authentication_ -q -- --test-threads=2`：3 项通过。未启用依赖隔离 DNS 的 ignored 测试。
- 依赖：`argon2 = 0.5.3`（std，默认含 password-hash，MIT OR Apache-2.0）、`bcrypt = 0.17.0`（默认 feature，MIT）、`base64 = 0.22.1`（默认 feature，MIT OR Apache-2.0）。`cargo tree --locked --offline -p argon2` 与 `-p bcrypt` 已检查；新增传递包为 base64ct 1.8.3、blowfish 0.9.1、byteorder 1.5.0、cipher 0.4.4、cpufeatures 0.2.17、inout 0.1.4、password-hash 0.5.0、zeroize 1.9.1。原锁定包未升级；base64 原已锁定。

## 独立复审后修复

- 准备阶段只保留令牌的 SHA-256 摘要；请求值受长度约束后计算摘要，并以 OpenSSL 对固定 32 字节进行比较，不按秘密长度提前返回。处理耗时仍取决于请求长度及其他处理，不承诺整个 HTTP 请求恒定时间。原令牌文件格式、跟踪和候选配额保持不变。
- `cargo test --locked --offline --test http_proxy authentication_ -q -- --test-threads=2`：6 项通过。新增真实服务测试以即时 `AUTH_CAPACITY` 确认四槽已占，再断开四个客户端并确认仍即时返回 `AUTH_CAPACITY`；不同长度、相同长度错误令牌、空及超长令牌均拒绝。此 HTTP 测试证明断连后的容量行为，不单独证明服务器 future 已取消。
- `cargo test --locked --offline --lib cancelled_authorize_keeps_running_worker_permits -q`：1 项通过。使用真实 bcrypt 工作任务，四槽占满后实际取消四个鉴权 future，确认容量不提前释放且新任务被拒，直到底层任务结束许可全部恢复；该测试仍依赖 cost-14 在测试机器上的实际运行时间。
- 本轮修复后再次运行 `cargo fmt --all --check`、`cargo check --locked --offline -q`、`cargo clippy --locked --offline --all-targets -q -- -D warnings`、`cargo test --locked --offline --test configuration -q`（36 项）、`cargo test --locked --offline --test reload authentication_ -q -- --test-threads=2`（3 项）和 `cargo build --release --locked --offline -q`：通过。非 root 全套及剩余独立复审须在本修复后重跑。

新增传递依赖逐项核对（用途按上述离线 `cargo tree` 和 crate 本地源码，许可证按对应版本本地 registry 的 `Cargo.toml` 元数据）：

| crate | 传递用途 | 许可证 |
| --- | --- | --- |
| base64ct 1.8.3 | Argon2/password-hash 的 PHC Base64 编解码 | Apache-2.0 OR MIT |
| blowfish 0.9.1 | bcrypt 使用的 Blowfish 算法 | MIT OR Apache-2.0 |
| byteorder 1.5.0 | blowfish 的字节序读写 | Unlicense OR MIT |
| cipher 0.4.4 | blowfish 的分组密码接口 | MIT OR Apache-2.0 |
| cpufeatures 0.2.17 | Argon2 的 CPU 特性检测 | MIT OR Apache-2.0 |
| inout 0.1.4 | cipher 的输入/输出缓冲区适配 | MIT OR Apache-2.0 |
| password-hash 0.5.0 | Argon2 的 PHC 哈希解析和验证接口 | MIT OR Apache-2.0 |
| zeroize 1.9.1 | bcrypt 默认 feature 的敏感缓冲清除 | Apache-2.0 OR MIT |

## 最终独立复审与质量门禁

独立 reviewer 完成哈希参数、凭据准备、头保护、暂停/恢复游标、动作成功缓存、watch/Reload、跨代配额、脱敏及依赖差异的源码复核。令牌按秘密长度短路的问题改为固定 32 字节摘要比较；补齐真实 HTTP 断连及直接取消鉴权 future 两层证据，八项传递依赖记录亦已核对。最终结论为 `OK with notes`，没有未解决的代码发现。reviewer 未自行执行测试，以下为父会话实际运行结果。

最后一次代码修改之后，父会话执行并通过：

```sh
cargo fmt --all --check
cargo check --locked --offline -q
cargo clippy --locked --offline --all-targets -q -- -D warnings
cargo build --release --locked --offline -q
cargo test --locked --offline --doc -q
git diff --check
```

非 root 全套在独立 0755 工作区 `/var/tmp/rill004-suite-EWT6UQ` 中执行：复制当前源码、配置及测试输入，以 root 运行 `CARGO_TARGET_DIR=/var/tmp/rill001-closure-suite-bGidmR/target cargo test --locked --offline --no-run --message-format=json`，从 Cargo JSON 选出 `profile.test=true` 的八个可执行工件，逐个运行 `runuser -u nobody -- <executable> --test-threads=2`。没有改变仓库共享 target 符号链接。

| 工件 | 通过 | 忽略 |
| --- | ---: | ---: |
| lib | 41 | 0 |
| main | 0 | 0 |
| configuration | 36 | 0 |
| http_proxy | 37 | 0 |
| https | 20 | 0 |
| reload | 34 | 1 |
| static_files | 27 | 0 |
| feasibility | 31 | 0 |

最终合计 **226 passed、0 failed、1 ignored**。忽略项要求 `127.0.0.1:53` 专用解析器，未启用。修复前首轮为 224 通过、0 失败、1 忽略，不代替最终结果。[原始全套输出](data/018-authentication/suite.log)包含真实取消测试、HTTP 断连测试、密码容量与所有权限回归结果。

实际工具链为 rustc 1.99.1、cargo 1.99.1；没有验证 MSRV 1.95。release 为 x86-64 动态链接 ELF，不声称完全静态。密码取消和容量测试依赖本机 cost-14 的实际耗时，不宣称任意性能环境都具有相同延迟；单次库运算不可抢占，超时后仍占真实验证额度。

模块边界为 `auth/model.rs` 凭据声明与脱敏、`hash.rs` 格式/成本、`token.rs` 受限读取、`verify.rs` 请求验证与工作许可；`config/parser/chain_auth.rs` 负责严格语法，现有策略编译管线负责准备、头注册与配额，执行器及代理适配负责暂停/恢复。未新增测试框架、模拟、夹具工具或网络获取，未操作生产 Caddy，未恢复 OTLP/metrics。用户已明确确认超长 bcrypt 密码及同动作重入语义，未修改冻结成本/并发/期限数值。WebDAV 等后续功能不在本提交内，G02 仍等待 008。
