# 001 gRPC 容器隔离验收

## 范围与结论

按用户“起个 grpc 测试容器验收”的指示，补充 001-T03 的协议可行性证据。基线为 `5a9784e`，没有实现 005 的生产功能，也未修改运行中的服务。

真正的 tonic/prost gRPC 后端在 Podman 容器内运行；客户端先直连对照，再经过独立 Pingora 0.9.0 适配原型调用。h2c 和 TLS h2 均通过一元、客户端流、服务端流、双向流，以及 trailers、取消、deadline 检查。错误名称和错误 CA 均被上游 TLS 校验拒绝。完整矩阵连续三轮通过，每轮 38 项判定，共 114 项。

这里不是 `rill run` 的端到端功能验收。生产配置、同一连接池中不同协议/目标/信任身份的隔离、跨 Reload、请求 trailers、正则正文变换等尚未验证；001-T03/AC03 及用户验收保持未完成。

## 工具与模块

目录：`tests/feasibility/grpc_container/`，独立 Cargo 工作区，有自己的 `Cargo.lock`；主工程 Cargo.toml/Cargo.lock 不变，不新增生产依赖。

| 文件 | 职责 |
| --- | --- |
| `run.py` | 离线构建、临时测试证书、容器/代理生命周期、六条场景及清理 |
| `probe.proto` | 四类 RPC 与 Packet protobuf 消息定义 |
| `src/main.rs` | backend/proxy/client 模式分发 |
| `src/backend.rs` | tonic h2c 与 TLS h2 监听 |
| `src/service.rs` | tonic Grpc 服务适配、prost 消息、取消计数 |
| `src/proxy.rs` | 隔离 Pingora ProxyHttp 原型，强制上游 h2，严格验证 CA/名称 |
| `src/client.rs` | tonic 客户端与协议断言 |

不需要 protoc/grpcurl：服务适配是手写 tonic Service，消息由 prost derive 编解码，形状与 `probe.proto` 对应，不是假 HTTP/2 回显。失败会使进程非零退出。

主要直接依赖均精确 pin：Pingora 0.9.0（Apache-2.0，openssl/proxy）、tonic 0.14.9（MIT，默认及 tls-ring）、tonic-prost 0.14.9（MIT）、prost 0.14.4（Apache-2.0）、Tokio 1.53.3（MIT，io-util/macros/net/rt-multi-thread/signal/sync/time）。完整依赖以独立 Cargo.toml/Cargo.lock 为准，均来自现有本地 Cargo 缓存；未执行网络下载或 cargo update。tonic 自身的传递追踪依赖仅用于该独立工具，不恢复 Rill 的观测导出功能。

## 环境与隔离

- rustc/cargo 1.99.1，Podman rootful，netavark。
- 已缓存镜像 `docker.io/library/debian:trixie-slim`，本轮实际 image ID：`394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853`。以解析后的 ID 运行，`--pull=never`。
- 每次创建独立 bridge，`isolate=true`，与其他容器网络隔离；只发布 `127.0.0.1` 随机端口。该选项不等于禁用全部出站连接；测试流量仅访问 loopback，未联系外部服务。
- 容器 UID/GID 65534、只读根文件系统、cap-drop=all、no-new-privileges、256 MiB 内存、1 CPU、64 PID。只读挂载测试二进制与临时证书目录。
- 临时目录位于 `/var/tmp`，父目录 0700。CA 私钥不挂载到容器，容器仅能读取本次生成的一日有效测试叶证书/私钥。所有私钥在 finally 中删除，未写入仓库。
- 主机代理只允许 loopback 入口与目标；每个代理独立进程，最多 16 个上游 h2 流，连接 3 秒、读写 5 秒；客户端整体用例 40 秒、进程 50 秒上限。
- 清理会停止本次代理进程、删除本次容器和网络；保留无私钥的日志、公开测试证书与 result.json，不操作其他容器。复审修正了按名称清理的冲突风险：现在先 create 并保存返回的容器 ID，再按该 ID 启动、查询和删除；创建失败不会按预期名称删除既有容器。

## 复现

仓库根目录运行，前提是依赖和镜像已在本地缓存：

```sh
python3 tests/feasibility/grpc_container/run.py \
    --image 394ffa2d2ac8f5c9f45148f22bad3587429bceb19c93b88e8595f1e3f7892853
```

脚本输出 `artifacts=/var/tmp/rill-grpc-acceptance-...`；在该目录查看各场景日志、后端日志、代理日志及 `result.json`。重复运行会重新选择端口、名称和证书，不复用上次资源。

实际构建命令：

```sh
CARGO_TARGET_DIR="$(readlink -f target)" cargo build \
    --locked --offline --manifest-path tests/feasibility/grpc_container/Cargo.toml
```

第一次尝试在 `/tmp` 编译，因 tmpfs 满导致 OpenSSL 归档失败；仅清理了本次失败的临时构建目录，随后改用仓库现有磁盘缓存成功，未删除/替换 target 符号链接。第一次容器尝试使用 internal 网络，host 发布端口调用超时；直连主机后端对照通过，改为独立 bridge 后容器矩阵通过。两次失败均不作为成功证据。

## 矩阵与断言

四条正常路径：直连容器 h2c、直连容器 TLS h2、经 Pingora 到 h2c、经 Pingora 到 TLS h2。后两条的客户端到代理均为 h2c，代理到后端由 PeerOptions::set_http_version(2, 2) 指定；TLS 由本次 CA 与 `grpc.test` 校验。

| 每条正常路径的检查 | 断言 |
| --- | --- |
| 一元 | protobuf text/count 完整往返 |
| 大消息 | 512 KiB protobuf 字符串逐字节相同，覆盖窗口流控 |
| 客户端流 | 分三次、间隔 25ms 上传 one/two/three，返回 onetwothree 和 count=3 |
| 服务端流 | 三条顺序消息，随后正常 EOF；tonic 验证成功结束状态并消费 trailers |
| DATA 后错误 trailers | 第一条消息正常，随后 DataLoss=15，grpc-message 为 `stream failed ü` |
| 双向流 | 上传 Sender 尚未关闭时逐次收到三条响应；每次 2 秒预算；关闭上传后正常 EOF |
| trailers-only | InvalidArgument=3、grpc-message 为 `probe error ü`、x-proof=trailers-only |
| 客户端取消 | 收到长流第一条后 drop 流；3 秒内后端取消计数增长，同一客户端 channel 上后续 RPC 成功 |
| deadline | grpc-timeout=150ms；取消或 deadline 状态，3 秒内后端取消计数增长，后续 RPC 成功 |

取消计数仅为工具进程内的可读测试状态，不是新增 Rill metrics。channel 后续成功证明兄弟 RPC 未失效，不宣称验证了底层同一 TCP 连接身份。

另外两条负面路径：相同 TLS 后端、错误验证名称，以及正确名称但不相关 CA。客户端必须得到 Unavailable（映射自 HTTP 502），代理日志必须包含 `Upstream InvalidCert`；不是把任意客户端错误都算作 TLS 拒绝。

## 实测输出

最终源码连续三轮，日志分别位于：

- `/var/tmp/rill-grpc-acceptance-a15slisg/`
- `/var/tmp/rill-grpc-acceptance-0jath7z7/`
- `/var/tmp/rill-grpc-acceptance-03nc586w/`

三轮完整原始输出保存在 [round-1.log](data/014-grpc-container/round-1.log)、[round-2.log](data/014-grpc-container/round-2.log)、[round-3.log](data/014-grpc-container/round-3.log)。日志只含测试地址、文件路径、判定和清理结果，不含私钥材料。

每轮 4 × 9 + 2 = 38 条 PASS。首轮原始输出摘录：

```text
PASS http://127.0.0.1:59103 unary
PASS http://127.0.0.1:59103 512KiB flow-control
PASS http://127.0.0.1:59103 client-stream
PASS http://127.0.0.1:59103 server-stream and success trailers
PASS http://127.0.0.1:59103 grpc-status=15 and grpc-message after DATA
PASS http://127.0.0.1:59103 bidi responses before request EOF
PASS http://127.0.0.1:59103 trailers-only grpc-status=3 and grpc-message
PASS http://127.0.0.1:59103 cancel reaches backend and sibling RPC succeeds
PASS http://127.0.0.1:59103 deadline cancels backend and sibling RPC succeeds
PASS http://127.0.0.1:35149 upstream TLS rejection: code: 'The service is currently unavailable', message: "grpc-status header missing, mapped from HTTP status code 502"
```

错误验证名称的代理日志摘录：

```text
proxy failure: Upstream InvalidCert context: TLS connect() failed: ... certificate verify failed ... SNI: wrong.test
```

各轮 `result.json` 均包含六个成功场景、`passed=true`、`cleanup=true`、`cleanup_errors=[]`。结束后 Podman 中没有 `rill-grpc-acceptance-*` 容器或网络，临时目录无 `*.key` 文件。

## 检查结果与剩余边界

实际通过：主工程与独立工具的 fmt --check、clippy --all-targets -D warnings；主工程 check --locked --offline、feasibility 25/25；容器矩阵三轮 114/114。独立 reviewer 初审发现容器名称冲突时的清理风险，修复并重跑矩阵后复审通过，无其他发现。

这补上了 001 的四类调用及 TLS h2 原型证据，但不勾选整个阶段：同一代理进程的多目标/多信任身份池隔离、跨 Reload、请求 trailers、正则捕获分块无关性、WebDAV 与日志的完整资源交接等仍需继续验证；也未替代用户对契约和限额的明确验收。
