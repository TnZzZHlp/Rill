# Caddy 能力补齐契约

## 1. 基线与排除项

本文件是 001-T02 提交用户验收的契约，不是实现或可行性证明。001-T03 的机制隔离验证与独立复审已通过（[015 收口记录](../../validation/015-caddy-contracts-feasibility-closure.md)），001-T04 用户在最终验收请求后明确回复“验收通过”，本契约及第 9 节清单已确认冻结；后续阶段仍需单独实施授权。

基线 HEAD 为 `450722f`，原 rill-web-server 012 完成提交为 `c5e6fa7`；记录的 rustc/cargo 为 1.99.1，未验证 1.95。`target` 是指向外部共享缓存的符号链接，不得删除或替换。观测功能移除后的源码只有 logging、access、identity、owner、request；没有 metrics、OTLP、tracing 代码，配置依现行 `docs/configuration.md` 拒绝相应字段。不得恢复这些功能。

差距文档中的 25 个域名来自磁盘配置调查，未逐一在线验证；“约 12 个可迁移”没有逐站验收；606 MiB 是目录大小，不是响应正文大小；并未扫描所有静态 root 的 sidecar。历史 RouteId 是 SHA-256 前缀渲染为 32 个十六进制字符，而非 16 个；当前 metrics 已不存在。这些历史描述均不能作为交付证据。

2.11 全部排除；OTLP、tracing、metrics 全部排除，2.10 仅日志身份。排除生产切换、Caddyfile、CEL、PCRE 回溯引用及断言、CONNECT/SOCKS、负载均衡、健康检查、ACME、HTTP/3、插件及分布式配置。通用 gRPC 在批准范围内，Pingora 原型四类调用与池政策机制已由 014/015 实测，生产功能由 005 实现。冻结验收清单见第 9 节，须逐项接受或拒绝。

## 2. 差距追溯矩阵

| 差距 | 批准范围 | 责任阶段 | 契约章节 | 非目标说明 |
| --- | --- | --- | --- | --- |
| 2.1 | 请求/响应头、Vary、Location、可信客户 IP、固定上游 Host 与 MIME 条件 | 003 | 3.3、4 | 排除通用头动作覆盖受保护安全头、Host/authority 或分帧 |
| 2.2 | 静态 respond、预检及健康响应 | 002 | 3.4、4 | 排除任意脚本执行 |
| 2.3 | rewrite、前缀、try-files、静态文件终止动作及错误链 | 002、003 | 3.5、4、7 | 排除改变旧 root 路由及其静态安全语义 |
| 2.4 | 类型化请求布尔匹配及错误链状态匹配 | 002、003 | 3.2、4 | 排除完整 CEL 和任意表达式执行 |
| 2.5 | 固定上游、Host、h2c/TLS h2、通用 gRPC；可行性待 001-T03 | 003、005 | 3.6、6 | 排除负载均衡及健康检查 |
| 2.6 | Basic、静态令牌与受鉴权保护的 WebDAV | 004、008 | 3.7、3.8、6 | 排除通用身份平台和 WebDAV 共享锁 |
| 2.7 | 受地址策略约束的动态代理与顺序正文替换 | 006、007 | 3.9、3.10、5 | 排除 CONNECT、SOCKS 和 PCRE 回溯引用及断言 |
| 2.8 | gzip/zstd 协商编码及既有静态 sidecar | 007 | 3.10、4 | 排除变换 gRPC、SSE、WebSocket；gRPC 可行性待 001-T03 |
| 2.9 | 按站点文件访问日志、轮转及字段白名单 | 009 | 3.11、5、6 | 排除任意头、凭据和 Cookie 等敏感字段导出 |
| 2.10 | 配置来源的可读日志站点与规则身份 | 009 | 3.11、6 | 排除指标身份映射、metrics、OTLP 和 tracing |
| 2.11 | 无，全部排除 | 无 | 1 | 排除该项全部能力，不新增对应运行时功能 |

## 3. KDL 契约

以下是拟实施的精确语法，非当前已支持的语法。KDL v2 使用 `#true`/`#false`；未知节点、属性和参数拒绝，错误码 `CONFIG_UNKNOWN`。路径相对声明它的文件；Profile 仍只允许现有五个超时/WebSocket 字段，Snippet 仍只展开旧 route，来源和引用 span 不丢失。示例中的路径和值均为占位示意，不是真实秘密。示例片段位于现有合法 `site` 内，除非注明其他位置；其余必需的 server/TLS 仍遵循现有配置契约。

### 3.1 模式与链

`chain` 为 site 的唯一新模式入口，包含按声明顺序排列的 `rule`；`rule` 接受可选静态标识作为第一个字符串参数，子节点依次为可选 `match` 和动作。没有 match 表示真。一个 rule 有一个终止动作，或有若干非终止动作并以 `continue` 显式结束。没有命中的链返回 404。site 的 `chain` 与 `route`、`use-snippet`、site 简写 `proxy`/`root` 互斥，混用返回 `POLICY_MODE_CONFLICT`；不得根据展开结果为空绕过检查。旧模式及其精确优先/最长前缀不变。

接受：
```kdl
chain {
    rule "health" {
        match { method "GET"; }
        respond 200 text="ok"
    }
}
```
拒绝 `chain { rule { respond 200; }; }; route "/" { root "/srv"; }`：`POLICY_MODE_CONFLICT`。空链、重复标识、多个终止动作或缺少终止/continue 为 `POLICY_INVALID_CHAIN`。

### 3.2 匹配器

`match` 恰有一个表达式；`all`、`any` 有一个或多个表达式，`not` 恰有一个。叶子为 `method "GET"`、`header-equal "name" "value"`、`header-prefix "name" "prefix"`、`path-glob "/api/*"`、`path-regex "^/api/"`、`client-cidr "192.0.2.0/24"`、`error-status 404`。方法区分大小写；头名不区分大小写，值逐个比较且不拼接；路径取当前 RequestContext.uri 的已验证、尚未做静态文件单次解码的 path，不含查询；rewrite/strip-prefix/try-files 后以及 reenter 的后续匹配均看到更新后的当前 path。original_uri 仅用于入口诊断与日志，不参与重入匹配；glob 的 `*` 可跨 `/`、`?` 匹配一个字符，反斜杠转义字面字符。客户 IP 来自现有可信代理解析。`error-status` 仅在错误链有效。正则采用 Rust regex 左最先匹配，无回溯引用或前后向断言。

接受：
```kdl
match {
    all {
        method "GET"
        any { path-glob "/api/*"; header-prefix "accept" "text/"; }
        not { client-cidr "192.0.2.0/24"; }
    }
}
```
拒绝 `match { not { method "GET"; method "POST"; }; }`：`POLICY_INVALID_MATCHER`。

### 3.3 头部

非终止动作 `header` 的首参数为 `request` 或 `response`；块中操作依次为 `set "name" "value"`、`add "name" "value"`、`delete "name"`、`replace "name" "literal" "replacement"`、`vary "token"`。`vary` 仅响应侧，按大小写不敏感去重。响应块可使用 `when-content-type="text/html"` 按不含参数的 MIME 类型匹配，及 `when-status="200..399"` 按闭区间匹配最终状态；两者同时出现时均须满足，状态范围端点为 200–599 且起点不大于终点，否则 `POLICY_INVALID_RESPONSE`。请求块不允许这些属性；`match` 仅匹配请求，错误链的 `error-status` 除外，不提供响应状态叶子。Location 使用同一 set/replace，不改变连接目标。`real-client-ip="X-Real-IP"` 仅为 `header "request"` 的属性，只允许目标字段 `X-Real-IP`（大小写不敏感），其他名称以 `POLICY_PROTECTED_HEADER` 拒绝；值来自现有客户地址解析。可信代理列表为空时使用真实对端地址，非空时仅对可信直接对端解析转发链，不额外信任原始 X-Real-IP。普通 header 块对 Host 或 authority 的操作均以 `POLICY_PROTECTED_HEADER` 拒绝；上游 Host 改写只允许固定 `proxy` 的 `host` 属性。值不支持任意环境变量；只允许字面值和 `{client-ip}`、`{upstream-authority}`、`{path}`、`{site}`、`{capture.1}` 至 `{capture.9}`，替换后校验头值。

接受：
```kdl
header "response" when-content-type="text/html" {
    set "Location" "/login"
    add "Cache-Control" "private"
    delete "X-Old"
    replace "X-Example" "old" "new"
    vary "Accept-Encoding"
}
continue
```
拒绝 `header "response" { set "Content-Length" "3"; }; continue`：`POLICY_PROTECTED_HEADER`。

003 实施补充确认：`{upstream-authority}` 是配置中已验证连接目标的规范 `host:port`，始终显式包含端口，IPv6 加方括号；不是 DNS 候选地址，也不受固定 `proxy` 的 `host` 覆盖影响。用户明确同意：动作执行时尚未确定上游则使用该变量明确失败，不填空、不猜目标、不重放规则。延后响应头动作在代理目标确定后可使用此值；respond/static 没有上游时使用同样失败。

### 3.4 响应

`respond STATUS text="UTF-8" content-type="MIME"` 为终止动作；text 默认空，content-type 默认 `text/plain; charset=utf-8`。状态仅允许 200–599，204/304 仅允许空文本。HEAD 计算与 GET 同样的表示头但不发送正文。

接受 `respond 401 text="unauthorized" content-type="text/plain; charset=utf-8"`。
拒绝 `respond 204 text="body"`：`POLICY_INVALID_RESPONSE`。

### 3.5 路径、分支与错误

`rewrite "/path"` 替换路径并保留原查询，目标不能自带查询或 fragment；`strip-prefix "/api"` 要求路径段边界匹配，不匹配产生 `POLICY_PATH_MISS`（LocalNotFound/404，可进入错误链），不继续下一规则；精确剥完路径时得到 `/`。两者非终止，重新校验路径，显式 `reenter` 从链头开始并消耗预算。rewrite/try-files 候选可使用 `{path}` 与 `{capture.1}`–`{capture.9}`，分别取当前已验证路径和当前成功规则捕获；其他变量为 `POLICY_INVALID_TEMPLATE`，展开后仍执行完整路径与 root 约束验证，不二次解码。`try-files root="/srv/www" "/a" "/index.html"` 依次在约束 root 下检查普通文件，首个存在则将路径改为该项并保留原查询；仅 Missing 继续尝试下一个候选，所有候选 Missing 时产生 `POLICY_FILE_MISS`（LocalNotFound/404，可进入错误链），不保留原路径继续。其他 IO/权限错误不能按 Missing 处理。`branch` 内顺序排列 `when { match {...}; ... }` 和最多一个末尾 `otherwise {...}`，只执行首个匹配分支；分支必须终止或显式 continue。`handle-errors` 是 chain 的可选末尾节点，包含 rule，不能嵌套 handle-errors；仅处理第 7 节可捕获的本地错误。

接受：
```kdl
chain {
    rule "files" {
        strip-prefix "/assets"
        try-files root="/srv/www" "/index.html" "/404.html"
        continue
    }
    rule { respond 200 text="ok"; }
    handle-errors { rule { match { error-status 404; }; respond 404 text="missing"; }; }
}
```
链内以终止动作 `serve-files root="/srv/www" { index "index.html"; spa #true; cache-control "public, max-age=60"; precompressed #true; browse #false; }` 取代旧 route 的 root 动作；它复用既有受约束 openat2 root、表示、Range 与 sidecar 语义，不改变 `spa`、`cache-control`、`precompressed`、`browse` 的既有验证含义。`index` 为单个文件名，默认 `index.html`，不得包含路径分隔符或为 `.`、`..`；每动作至多一个。

接受：
```kdl
chain { rule { serve-files root="/srv/www" { index "index.html"; spa #true; cache-control "public, max-age=60"; precompressed #true; browse #false; }; }; }
```
拒绝：
```kdl
chain { rule { serve-files root="/srv/www" { index "../index.html"; }; }; }
```
返回 `POLICY_INVALID_PATH`。拒绝 `rewrite "https://example.org/"`：`POLICY_INVALID_PATH`。`reenter` 为终止当前遍历的专用控制动作，不能和 continue 同处一规则。

分支接受：
```kdl
chain {
    rule "api" {
        branch {
            when { match { path-glob "/api/*"; }; proxy "http://127.0.0.1:9000"; }
            otherwise { serve-files root="/srv/www"; }
        }
    }
}
```
分支拒绝 `branch { otherwise { respond 200; }; when { match { method "GET"; }; continue; }; }`：`POLICY_INVALID_BRANCH`（唯一原因是 `otherwise` 不是最后一项）。分支缺终止或缺少 `continue` 用 `POLICY_INVALID_CHAIN`，与链级规则一致。`try-files` 候选含 `..` 或候选数为 0：`POLICY_INVALID_PATH`；`handle-errors` 嵌套：`POLICY_INVALID_CHAIN`。

### 3.6 固定上游

`proxy "http://127.0.0.1:8080" protocol="http1" host="backend.example"` 为终止动作。protocol 可为 `http1`、`h2c`、`tls-h2`，默认 http1；h2c 仅 http，tls-h2 仅 https。host 是唯一允许的上游 Host/authority 覆盖位置；dynamic-proxy 不允许 host 属性。host 改写 HTTP Host/authority，不改 TLS 证书验证名；Pingora h2/gRPC 可行性待 001-T03 隔离验证；连接池按协议、连接目标、TLS 验证名及信任身份隔离。只在连接失败且尚未发送时按原有地址预算尝试；不重放已发送请求。

接受 `proxy "https://backend.example:443" protocol="tls-h2" host="service.example"`。
拒绝 `proxy "http://backend.example" protocol="tls-h2"`：`POLICY_UPSTREAM_PROTOCOL`。

### 3.7 鉴权

`basic-auth realm="private" { user "alice" hash="..."; }` 和 `static-token header="X-Api-Token" file="/secure/token"` 是非终止动作，必须在受保护终止动作之前，同一路径不能绕过。Basic 只允许 bcrypt `$2b$` 和 Argon2id PHC；令牌为独立普通文件的单行 UTF-8 值，准备时读取且拒绝组/其他用户可读文件；凭据不输出日志。缺失、重复、畸形或错误凭据均终止 401；Basic 返回 challenge。成功后移除入口 Authorization、Proxy-Authorization 和令牌头，禁止通过普通 header 恢复认证身份。文件路径保留来源。

2026-11-05 用户确认：bcrypt 解码后的密码超过 72 字节时不截断，执行受相同四槽、两秒限制的同成本 dummy bcrypt 后返回 401；Basic 输入上限仍为 4096 字节，Argon2 不增加 72 字节限制。成功结果仅可在同一请求、同一快照及同一鉴权动作重入时复用；不同动作、请求或代次不复用，失败不缓存。

接受：
```kdl
basic-auth realm="private" { user "alice" hash="$2b$12$abcdefghijklmnopqrstuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu"; }
continue
```
拒绝 `basic-auth realm="private" { user "alice" hash="plaintext"; }`：`AUTH_INVALID_HASH`。示例哈希仅说明格式，实际部署必须使用有效哈希。

静态令牌接受：
```kdl
static-token header="X-Api-Token" file="/secure/token"
continue
```
拒绝 `static-token header="Authorization" file="/secure/token"`：`AUTH_PROTECTED_HEADER`（令牌头不能是被保护或协议拥有的头）。拒绝不可读、可被组/其他用户读取、含多行或超过 4096 字节的令牌文件：`AUTH_INVALID_CREDENTIAL`。

### 3.8 WebDAV

`webdav root="/srv/dav" storage-id="documents"` 是终止动作，必须由同一路径上成功的 basic-auth 或 static-token 保护；root 必须显式配置。支持 OPTIONS、PROPFIND、PROPPATCH、MKCOL、GET、HEAD、PUT、DELETE、COPY、MOVE、LOCK、UNLOCK；独占写锁及持久属性，不支持共享锁。Destination 必须同一存储、受约束解析；禁止外部 XML 实体。PUT 在同目录临时文件完成后原子 rename；失败清除临时文件。MOVE 跨文件系统失败，COPY/MOVE 多文件与属性不承诺跨文件事务；属性单资源原子替换，失败返回错误，不假称整体原子。

接受：
```kdl
basic-auth realm="dav" { user "alice" hash="$2b$12$abcdefghijklmnopqrstuuuuuuuuuuuuuuuuuuuuuuuuuuuuuuu"; }
webdav root="/srv/dav" storage-id="documents"
```
拒绝未受保护的 `webdav root="/srv/dav" storage-id="documents"`：`DAV_AUTH_REQUIRED`。

008 补强实施已提供显式请求级真实目录项 Proof、共享遍历 ledger 和持久 inode 私有标记；仍待独立复审/最终验收，见 [022](../../validation/022-caddy-webdav.md)。不以普通 FS 回归冒称 casefold 专用环境或所有崩溃窗口已实测。

008 实施补充经用户明确确认：保留 `.rill-dav` 属性目录及 `.rill-dav-put-` 临时名前缀，任何服务入口（含旧静态快照、列表、缓存及解析后别名）不得读取内部数据。目录 0700/文件 0600 不替代同服务 UID 的读取屏障；既有同名用户目录不接管，check 不创建，失败候选只撤回自己的未发布资源。DAV 路径组件不允许 symlink，普通文件 nlink>1 拒绝；不同 storage-id 的同根/父子物理根（含已移除 ID 的历史身份）不得建立平行锁域。这些限制不改变一般旧静态根内安全 symlink 语义。无法证明物理根不重叠的 mount 别名安全拒绝。请求级 NamespaceSession 持 owned namespace guard，所有存在组件经父 FD 增量枚举确认真实 bytes 拼写与 dev/ino/type/mount；折叠但没有精确项的 alias 拒绝。source/target/tagged If 与整个请求各 walk 共用 4096 枚举/访问 ledger，根信用和私有非 dot 项不免费，不按目录/树重置；先完成 source 和旧 overwrite target 两树预检再删除。

Destination 及 tagged If URI 使用同快照的实际方法、入口头、客户 IP 和已成功鉴权动作做纯授权证明，终点须为同一 prepared DAV handle；路径变换/子树/href 同样要有明确映射，不重验密码、不重放外部副作用、不猜逆映射。仅出现 method/header 匹配不是拒绝理由。目的/子资源的新增解释扣同请求剩余 1024 策略步，不按每资源重置；首规则中可证明闭合的普通无条件/prefix 域与显式 strip-prefix、相同 prepared handle 可复用请求级证明。复杂策略可能在 4096 项前耗尽步骤，遍历项上限不是任意复杂政策下的成功保证。GET/HEAD 复用既有表示与 007 响应变换边界，写响应和安全/容量错误不被错误链改成成功。

用户合并表单 d4fd7a63 已明确批准：rustix =1.1.4（default-features=false、fs/std，Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT）及必要锁条目；固定持久 inode xattr 与同 parent O_TMPFILE 先标记/sync 再安全 link 保留名方案；cadaver 官方包/必要库仅临时隔离获取，最小 DAV HTTP 测试 helper，独立 private mount namespace 的 1 MiB tmpfs ENOSPC fixture。不追认此前检查点为安全完成，不允许宿主安装/apt upgrade/镜像拉取或其他新依赖。

内部 owner/property/temp 均带固定 user.rill.webdav.private 标记；退休不清，static 最终普通文件安全重开后以有界小栈 fgetxattr 查标，防退休/重启别名；目录用受约束 O_PATH 父链/固定名身份屏障，不新增所有普通 static GET 枚举限额或全面禁链。DAV activate 真实验证 xattr/O_TMPFILE/安全发布能力，缺失则 fail closed，不先命名后打标；check 不写能力探测。匿名发布可用已验证 procfd 且原 FD 保活的安全 link，并复验目标身份，不以 proc 路径重新授权客户端路径。旧未 tag 检查点格式拒绝接管，须隔离/清理，不承诺任意历史硬链接自动识别。

PUT rename/父 sync 后清标记/file sync；清标记失败/崩溃可留下已提交但不可读目标，结果 unknown，不回滚/自动重试。管理员确认完整性后显式清固定 xattr/fsync 恢复，不新增 CLI。进程内退休记录保真实 FD 和现有预算，链接仍在则不丢屏障或提前归还临时空间，直到 nlink0 回收，不留永久裸 inode tombstone。

### 3.9 动态代理

`dynamic-proxy prefix="/fetch/" anonymous=#false { allow-port 80; allow-port 443; }` 为终止动作。prefix 后是一次 URL 解码的完整 http/https URL；默认仅端口 80/443，额外端口只由 allow-port 显式加入。anonymous 默认 false，必须事先鉴权；true 显式接受开放代理滥用风险。拒绝 userinfo、非公网、特殊地址、自身入口及任一混合 DNS 禁止候选；每轮解析全部检查后固定连接 IP，不二次解析、不自动跟随 Location、不重放。移除入口 Authorization、Proxy-Authorization、Cookie；默认删除响应 Set-Cookie，并将指向同一目标的 Location 改写为此前缀内 URL，其他 Location 原样返回且不跟随。

2026-11-06 用户确认：当前 URI 的 prefix 后原始 path-and-query 整体只解码一次，不另行拼接或合并查询；相对 Location 先按目标 URL 解析，同 origin（scheme、规范 host、有效端口）才编码完整 URL 并加 prefix，其他原样返回且不跟随。每 origin 最多 8 条存活物理连接，包含握手中、空闲池及跨 Reload 旧代，复用不额外计数，弱引用失效后回收，满额拒绝。批准仅在隔离网络和 mount namespace 中以 loopback 模拟公网地址、私有 DNS 配置及临时 HTTP/HTTPS/DNS 服务验证，不允许访问真实公网或云元数据。

接受 `dynamic-proxy prefix="/fetch/" anonymous=#true`。
拒绝无前置鉴权的 `dynamic-proxy prefix="/fetch/"`：`DYNAMIC_AUTH_REQUIRED`。

### 3.10 替换及编码

`body-replace mime="text/html"` 包含按序的 `literal "old" "new"` 或 `regex "pattern" "replacement"`；正则替换支持 `$1` 至 `$9` 及 `$$`；不存在的捕获展开为空。其他 `$` 形式、命名捕获替换及两位编号以 `POLICY_INVALID_REPLACEMENT` 拒绝，避免 `$1name` 的词法歧义，捕获后的字面文字必须以非字母数字/下划线分隔。`encode mime="text/html" { gzip level=3; zstd level=3; }` 启用协商，两者至少一个。均为非终止响应过滤声明；body-replace 可在同一链中多次声明，encode 在主链与错误链合计最多声明一次，超出以 `POLICY_LIMIT` 拒绝；仅累积实际执行路径上、终止前已执行的声明，按执行声明顺序收集。它们作用于最终终止动作的响应，包括 serve-files、proxy、respond 和错误链响应；进入错误链后保留此前已执行的声明，再追加错误链实际执行的声明，错误链未执行的声明不生效。所有 body-replace 内 literal/regex 合计每响应最多 16 条，跨声明及错误链累计；编译时对可能路径超限拒绝 `POLICY_LIMIT`，运行时仍执行同一上限。所有替换按收集顺序运行，再应用唯一已执行的 encode 协商。MIME 必须显式列出，可重复 mime 属性以外的节点 `mime "type"` 扩展；不支持通配。流式策略及例外见第 4 节。无界正则经 regex-syntax HIR 的 maximum_len=None 判定后使用保守 EOF 回退：逐块累积尚不可判定的输入；每请求全部 pending、替换、中间、交接存储及工作信用共享 1 MiB，无界 pending 可用量须扣除同时存活的工作缓冲，不保证单条规则或 pending 独享 1 MiB。所有无界 stage 共用一次分配且按真实 capacity 全额计费的连续 arena，region 按实际使用划分、紧凑复用，不采用逐块整段双 allocation 扩容，也不使用固定重叠窗口；到 EOF 后在完整待决序列上执行同一次 Rust 左最先匹配及捕获展开，输出按不大于 64 KiB 的片段交接且累计上限 64 MiB。该回退可能完全不能提前输出，是 D06 已接受的边界，不允许为保证成功而无限聚合或落盘。字面量使用模式长度决定的待决前缀；有界正则必须保留其实际最大匹配长度与边界断言所需上下文，不能把字面量窗口当成任意正则的正确性依据。

接受：
```kdl
body-replace mime="text/html" { literal "old" "new"; regex "(cat)" "$1!"; }
encode mime="text/html" { gzip level=3; zstd level=3; }
continue
```
拒绝 `body-replace mime="text/html" { regex "(?=a)" "b"; }`：`POLICY_INVALID_REGEX`。

编码接受：`encode mime="text/html" { gzip level=3; }`。拒绝 `encode mime="text/html" { gzip level=9; }`：`POLICY_LIMIT`（level 只接受 3）。拒绝空声明 `encode mime="text/html" { }`：`POLICY_INVALID_ENCODE`（必须至少一个编码器）。

2026-11-06 用户补充确认：正文替换按 Rust `regex::bytes` 语义处理原始字节，默认 Unicode 规则遵循该库，允许显式关闭 Unicode 的字节正则；非法 UTF-8 不作有损解码或额外拒绝。literal 模式为配置中的 UTF-8 字节串；空模式在完整逻辑输入的每个字节边界匹配一次（包括开头和 EOF），不因输入分块重复，也不按 Unicode 字符边界调整。实际经过替换或在线编码的表示删除所有源 ETag（包括弱 ETag）、Last-Modified、Content-MD5 和原 Content-Length，不得用源验证器直接返回变换表示的 304。未变换、Range、no-transform、未匹配 MIME 及仅沿用静态 sidecar 保留原有缓存协商，但代理条件请求有以下明确例外：实际路径已登记正文替换或在线编码，且请求侧尚不能确定旁路的 GET/HEAD，在唯一一次上游请求副本中移除 If-None-Match 和 If-Modified-Since；最终 MIME 未命中、SSE 或响应 no-transform 时仍原样旁路正文，但该次响应可以从 304 变为 200。已确定的 Range/其他旁路及未登记变换保留既有条件语义；不得移除 If-Match、If-Unmodified-Since 等业务前置条件或 Range 路径的 If-Range，不得重放请求。静态资源按已确定的真实表示处理，不套用此上游例外。

### 3.11 文件日志与身份

site 内 `access-log`：`file`、`rotate-size`、`retain-files`、`retain-days` 为必需属性；数值分别接受 16 KiB–10 MiB、1–5、1–7（天），其他值为 `LOG_LIMIT`。`field` 可重复，白名单为 `timestamp`、`site`、`route`、`method`、`status`、`duration-ms`、`bytes`、`path`、`client-ip`。默认前七项，path/client-ip 只在明确配置时记录；不得记录任意头、凭据或 Cookie。`site-label` 为 site 静态身份，缺省使用规范化配置域名；rule 参数为 route 身份，未命名按配置顺序生成 `rule-N`。不能使用请求 Host/URI 作为身份。site 未配置 `access-log` 时回退现有全局 stderr 访问日志，由现有 observability access-log 设置控制；两者均配置时，站点文件日志和已启用的全局 stderr 日志各自输出，关闭全局设置不关闭站点文件日志。

接受：
```kdl
site-label "public-site"
access-log file="/var/log/rill/site.jsonl" rotate-size=10485760 retain-files=5 retain-days=7 {
    field "timestamp"
    field "site"
    field "status"
}
```
拒绝 `access-log file="/var/log/rill/site.jsonl" rotate-size=10485760 retain-files=5 retain-days=7 { field "authorization"; }`：`LOG_FIELD_FORBIDDEN`。

## 4. 执行顺序与语义

请求开始读取一次 `Arc<RuntimeSnapshot>`，直至流结束持有；旧请求不重读活动代次。按声明顺序短路求值，all 遇假、any 遇真停止，not 取反。每个实际访问的 matcher 节点先消耗共享 steps_left；预算不足立即失败，不计算未访问分支。规则在临时捕获集合中求值：false 全部丢弃，not 永不输出捕获；all 全真采用最后一个已成功求值的正向 path-regex 捕获，any 采用首个真分支捕获，无正则则为空。仅规则整体匹配后替换 RequestContext.captures；进入下一条成功规则或 reenter 时不能沿用旧捕获。延后响应过滤声明在登记时创建请求级 ResponseFilterInstance，冻结该规则捕获 Arc 值，不从后续规则读回已变更的捕获；同一组共享字节只计一次，旧实例仍持有的捕获在释放前继续占用 36 KiB 预算，重入追加实例也受 256 个及 64 KiB 元数据预算限制。每个求值节点、动作、分支进入和重入计入共同步数；首个匹配分支排他。规则按序执行；serve-files、respond、proxy、dynamic-proxy、webdav 终止；非终止序列必须显式 continue；reenter 从头开始且最多 4 次。嵌套和步数见第 5 节。错误链最多进入一次，错误链失败直接失败。

请求处理为匹配、鉴权、请求头/路径动作、终止动作；有资格的本地错误在响应头发送前进入错误链。响应头条件在终止动作产生最终状态和上游响应头后求值，MIME 匹配解析后的 Content-Type，不对请求条件提前猜测。先解码受支持的 Content-Encoding，再按声明顺序替换，再按 Accept-Encoding 协商编码，最后统一生成分帧和响应头；上游未知编码、`Cache-Control: no-transform`、Range、SSE、gRPC、WS 不变换；Pingora h2/gRPC 可行性仍待 001-T03 隔离验证。HEAD、204、304 不发送正文；HEAD 使用对应 GET 表示的可确定元数据，无法预知变换长度时不发送 Content-Length。变换移除原 Content-Length、强 ETag 和 Content-MD5；合并 Vary: Accept-Encoding；不可接受任何编码时 406。未变换静态 sidecar 维持既有协商与 Range 语义。变换后不得错误复用源表示缓存验证器。gRPC/SSE/WS、no-transform 与 Range 旁路依据终止动作/原始响应元数据判断，不能通过自定义响应头改 MIME 来绕过；响应条件在变换前求值，表示/framing 修正最后执行。静态 body-replace 命中时从 identity 表示读取，不因选中 br/gz sidecar 跳过已声明替换；仅 encode 命中而不替换时保留已有 sidecar 协商。

| 头类别 | 规则 |
| --- | --- |
| Content-Length、Transfer-Encoding、Content-Encoding | 通用头动作不能设置、追加、替换或删除；协议/正文适配层拥有分帧和编码，避免头与实际正文不一致 |
| Connection 及其列出的 token、Keep-Alive、Upgrade、TE、Trailer、Proxy-Connection | 通用头动作禁止；协议适配层拥有 |
| Host、`:authority` | 通用动作禁止；仅固定 proxy 的 host 属性，不能改变 TLS 身份 |
| Forwarded、X-Forwarded-* | 通用动作禁止，现有可信代理策略拥有 |
| Authorization、Proxy-Authorization、认证令牌头及身份头 | 通用动作不能设置、追加、替换；鉴权成功专用清除，不能删除或伪造身份以绕过鉴权 |
| Content-Type、Location、Vary | 可按本契约操作；值校验，Vary 合并 |

respond 的 Content-Length 由 UTF-8 字节数决定；HEAD 不发送体但在可确定时发送对应 GET 的长度；204 禁止 Content-Length 和正文；304 不发送正文，不凭空设置长度。默认文本类型为 `text/plain; charset=utf-8`。任何已提交头的失败不得再发送第二个响应。

## 5. 限额表

以下新限额为已获用户验收的硬上限，不是性能承诺；超限不能静默截断。配置期错误为 ConfigError code，请求期代码为内部脱敏失败码，状态见第 7 节。MiB/KiB 为二进制单位。

| 对象与作用域 | 上限/默认 | 失败码及执行模块 |
| --- | --- | --- |
| serve-files index 文件名 | 每动作 1 个，UTF-8 最多 1024 字节 | `POLICY_LIMIT`；config/http_policy |
| try-files 候选 | 每动作最多 64 个，候选路径各最多 4096 UTF-8 字节 | `POLICY_LIMIT`；config/http_policy |
| access-log 文件节点 | 每站点最多 1 个，全候选最多 64 个 | `LOG_CAPACITY`；config/observability |
| 每站点链规则 | 主链与错误链合计最多 4096 条 | `POLICY_LIMIT`；config/http_policy |
| 每候选编译策略 | 最多 8 MiB，含全部站点编译策略及正则程序，不含运行时 DFA 缓存 | `POLICY_LIMIT`；http_policy |
| 使用链的站点 | 每候选最多 128 个 | `POLICY_LIMIT`；config/http_policy |
| 链每请求步骤 | 1024 | `POLICY_BUDGET_EXCEEDED`；http_policy |
| 链嵌套深度 | 16 | `POLICY_LIMIT`；config/http_policy |
| 每链 matcher 节点 | 256 | `POLICY_LIMIT`；config/http_policy |
| 每正则 UTF-8 字节 | 4096 | `POLICY_INVALID_REGEX`；config/http_policy |
| 正则显式重复次数 | 1000；无界重复允许 | `POLICY_INVALID_REGEX`；config/http_policy |
| 每正则编译程序及 DFA 缓存 | 各 2 MiB；进程编译策略和 DFA 缓存分别最多 64 MiB，含候选及旧代次 | `POLICY_INVALID_REGEX`/`POLICY_LIMIT`；http_policy/body_transform |
| 每 glob 字节 | 1024 | `POLICY_LIMIT`；http_policy |
| rewrite 重入 | 每请求 4 次 | `POLICY_BUDGET_EXCEEDED`；http_policy |
| 替换规则 | 每响应 16 条 | `POLICY_LIMIT`；body_transform |
| 正文变换全部存储及工作信用 | 每请求合计 1 MiB，全进程 64 MiB；pending 可用量扣除同时存活工作缓冲 | `BODY_BUDGET_EXCEEDED`；body_transform |
| 解码总输出、膨胀 | 每响应 64 MiB、至多输入的 100 倍加 64 KiB | `BODY_BUDGET_EXCEEDED`；body_transform |
| 替换输出、CPU 与等待预算 | 每响应 64 MiB，CPU 完成预算 2 秒、执行等待 2 秒 | `BODY_BUDGET_EXCEEDED`；body_transform；CPU 在匹配/展开安全检查点累计，单次库调用不可抢占；超时不发布迟到结果，仍运行任务继续占用许可 |
| 变换并发 | 64 响应 | `BODY_CAPACITY`；body_transform |
| 压缩 | gzip level 3、window 32 KiB；zstd level 3、window 1 MiB；不接受其他 level | `POLICY_LIMIT`；body_transform |
| bcrypt | cost 10–14 | `AUTH_INVALID_HASH`；auth |
| Argon2id | v19，内存 8192–65536 KiB，迭代 1–3，并行度 1–2 | `AUTH_INVALID_HASH`；auth |
| 密码验证 | 全进程 4 个运行任务、不排队；每次 2 秒 | `AUTH_CAPACITY`/`AUTH_TIMEOUT`；auth；超时不释放仍运行任务的槽 |
| 令牌文件及 Basic 输入 | 各 4096 字节 | `AUTH_INVALID_CREDENTIAL`；auth |
| DAV 锁 | 每存储 1024，全进程 4096；最长 3600 秒 | `DAV_LIMIT`；webdav |
| DAV 属性 | 每资源 64 项、总计 64 KiB；XML 请求 1 MiB、深度 32 | `DAV_LIMIT`；webdav |
| DAV 遍历 | 每请求 4096 项、深度 16 | `DAV_LIMIT`；webdav |
| DAV 临时 PUT | 每文件 256 MiB，全进程 1 GiB；并发 16 | `DAV_LIMIT`；webdav |
| DAV 临时写入期限 | 300 秒 | `DAV_TIMEOUT`；webdav |
| 动态目标 | URL 4096 字节；代理跳数最多 1，不跟随重定向 | `DYNAMIC_TARGET_INVALID`；proxy |
| 动态代理资源 | 全进程 128 个活动请求、DNS 候选 64、每目标 8 连接 | `DYNAMIC_CAPACITY`；proxy |
| 池政策身份 | 全进程 4096 个，含候选与旧政策；唯一 u64 token checked_add 分配、不回绕复用 | `POLICY_LIMIT`；proxy/runtime |
| 政策生成的头及捕获 | 请求/响应头各 64 KiB；每组最多 9 个路径捕获，当前及冻结捕获合计每请求 36 KiB；响应过滤实例最多 256 个，元数据合计 64 KiB | `POLICY_LIMIT`/`POLICY_BUDGET_EXCEEDED`；http_policy |
| 日志队列 | 每站点 1024 条，全进程 16384 条；每条 16 KiB | `LOG_DROPPED`；observability |
| 日志站点/sink/worker | 全进程各 64；含候选及旧代次尚持有资源 | `LOG_CAPACITY`；observability |
| 日志 FD | 全进程 256，包含父目录、活动文件、候选与轮转交接 FD | `LOG_CAPACITY`；observability |
| 日志轮转 | 每文件 10 MiB；保留 5 个轮转文件、7 天；各配置不得超过此值 | `LOG_LIMIT`；observability |
| 日志磁盘 | 每站点最多 6 文件、60 MiB；全进程最多 384 文件、3840 MiB；每父目录扫描最多 4096 项 | `LOG_LIMIT`；observability |
| 日志关闭等待 | 5 秒；请求不等待磁盘 | `LOG_TIMEOUT`；observability |
| 固定上游 DNS | 缓存 60 秒，失败退避 1 秒，候选 64，建连尝试 8 | 既有地址/上游错误；proxy |
| 代理默认期限 | connect 5 秒、read 60 秒、request 300 秒、idle 60 秒；现有配置可覆盖 | 既有超时；proxy |
| 入口 | HTTP/1 首头 60 秒、解析头 64 KiB；接受槽 1024 | 既有入口错误；proxy |
| TLS/H2 | 握手 10 秒、H2 前言 10 秒、每连接 100 流、解码头 64 KiB、空闲 60 秒 | 既有协议错误；proxy |
| 静态缓存 | 默认 0，启动配置最大 256 MiB；单对象 64 KiB、1024 项 | 既有缓存旁路；static_files |
| 静态 IO | 全进程 128 槽 | 既有容量行为；static_files |
| 配置加载 | 单文件 1 MiB、总计 8 MiB、512 文件、导入深度 64、KDL 深度 32 | 既有配置错误；config |
| snippet 展开 | 深度 64、路由 4096、步骤 16384、payload 8 MiB | 既有配置错误；config |
| 静态错误页/列表 | 错误页 1 MiB；扫描 4096、展示 512、HTML 256 KiB | 既有静态错误；static_files |
| 控制面 | 帧 64 KiB；默认 reload 30 秒；首字节后帧 5 秒 | 既有管理通信错误；control |

2026-11-06 用户补充确认：正文每请求 1 MiB、进程 64 MiB 限额覆盖 Rill 自己持有的全部待决、替换、中间和交接缓冲，按实际分配 capacity 计量，包括输出的 Bytes 克隆仍持有的内存；不包括 gzip/zstd 库内部上下文及工作区，因此不是 codec 内部内存或进程 RSS 的字节硬上限。gzip 窗口 32 KiB，zstd 窗口 1 MiB，解码器设置 windowLogMax=20，禁止外部字典及多线程配置。64 响应许可覆盖 codec 生命周期；取消或超时后实际任务尚未结束时仍持有许可及缓冲额度。

007 独立复审后的用户最新确认“保留共享总额建议”：上述 1 MiB/64 MiB 仍是共享硬限；不保证 pending 或每条规则独享 1 MiB。原逐块全量双 allocation 导致约半 MiB 过早失败的实现不获追认，必须消除。共享 arena 的未用区域仍是实际 allocation，不能按 region 缩短伪造退券；工作信用在同一 RAII 额度内预留/分拆，未消费信用也影响准入，实物不双计。输入、decoded、所有 stage、bounded pending/KMP 表、静态源额外持留及全部输出 owner 均扣实际容量。普通输入信用按 64 KiB、decoded/stage 按 16 KiB、最低输出信用按 3×16 KiB 计算，但不是历史 H2 owner 上限，更多存活 owner 必须另占真实剩余额度或失败，不增等待。源大于输入信用则明确拒绝，不能隐藏底层；输入 EOS 后实际释放 source/decoder/decoded 和未用输入信用，再传播 EOF。PikeVM 在 region 完整逻辑 slice 上匹配，pull fragment 用相对 offset，复制到带券 stage Buffer 后才推进下级/紧凑；上级所有捕获片段完成后才释放 region。详见 021 本次实现与分块边界证据；此确认不等于此前实现已经满足契约。

2026-11-06 实施边界确认：上述存储预算不含原 HTTP 框架接收/解析/flow-control、framing/HPACK、TLS 与内核 IO 存储；不得将此说成整条 HTTP 或 RSS 硬限。proxy 当前框架 Bytes 必须在同步提交内先预留自有实际容量、复制后立即 drop，不能跨等待、worker 或 ctx 额外保留未知底层，不能拆片隐藏共享大 allocation。Rill 输出原 allocation 仍由 from_owner 许可覆盖任何框架 clone，直至真正释放；既有 h2 0.4.20 framed_write.rs:240–258 向原 encoder.buf 的小 DATA/前缀 framing 拷贝属于框架内部存储，不允许据此新增未计量 Rill 队列或提高窗口。respond 直接从候选文本一次复制；变换文件读取先分配预算 Buffer；旧 listing/cache producer 在确定变换后的额外持留分别按真实 Vec.capacity/Box 全容量收费，源/input 同时存活时同时计费，释放源实际存储/额外引用后才释放对应券。见 021 的源与输出生命周期证据。

### 5.1 PikeVM 工作区补充授权（2026-11-05）

用户批准独立 PikeVM 运行工作区全进程上限 64 MiB，包含候选和旧代仍持有的运行缓存。分配前按可证明上界预留，不排队，额度不足以 `POLICY_BUDGET_EXCEEDED` 失败，使用结束释放。合法正则配置可编译成功，而请求因工作区不足失败。此额度不改变编译策略 8 MiB/64 MiB 或 DFA 2 MiB/64 MiB 限额；PikeVM 不使用 DFA 缓存。库报告的 cache 内存不包含 epsilon 栈的 capacity，不能代替上界预留。

## 6. 跨模块接口与资源生命周期

以下是新增公开接口的 Rust 草图；名称、参数和所有权是实施契约，具体错误构造、异步实现和现有可见性需按模块落地。`ConfigError` 保留 `Origin` 及 related 来源；不新增第二条配置管线。

```rust
// src/http_policy/mod.rs
pub struct RuleChain { pub rules: Vec<Rule>, pub errors: Option<ErrorChain> } // 手动 impl Debug
pub struct Rule { pub id: Box<str>, pub matcher: Box<dyn Matcher>, pub actions: Vec<Box<dyn Action>> }
pub struct ErrorChain { pub rules: Vec<Rule> }
pub struct ExecutionBudget { pub steps_left: u16, pub reentries: u8, pub error_entered: bool }
pub struct ActionId(pub u32);
pub struct AuthId(pub u32);
pub struct FilterId(pub u32);
pub struct ResponseFilterInstance {
    pub filter: FilterId,
    pub captures: [Option<std::sync::Arc<str>>; 9],
}
pub enum AuthIdentity { Anonymous, User(Box<str>), TokenPolicy(AuthId) }
pub struct RequestContext {
    pub snapshot: std::sync::Arc<crate::runtime::state::RuntimeSnapshot>,
    pub site: usize,
    pub original_uri: http::Uri,
    pub uri: http::Uri,
    pub method: http::Method,
    pub headers: http::HeaderMap,
    pub client_ip: std::net::IpAddr,
    pub authenticated: AuthIdentity,
    pub captures: [Option<std::sync::Arc<str>>; 9],
    pub response_filters: Vec<ResponseFilterInstance>,
    pub error: Option<PolicyError>,
    pub budget: ExecutionBudget,
}
pub enum ResponseSource { Local, FixedProxy, DynamicProxy, WebDav }
pub struct ResponseContext {
    pub status: u16,
    pub headers: http::HeaderMap,
    pub source: ResponseSource,
    pub original_content_type: Option<http::HeaderValue>,
    pub committed: bool,
}
pub enum ActionResult { Continue, Reenter, Authorize(AuthId), Terminal(ActionId) }
pub struct MatchContext<'a> {
    pub method: &'a http::Method,
    pub path: &'a str,
    pub headers: &'a http::HeaderMap,
    pub client_ip: std::net::IpAddr,
    pub error_status: Option<u16>,
    pub steps_left: &'a mut u16,
}
pub struct MatchResult { pub matched: bool, pub captures: [Option<std::sync::Arc<str>>; 9] }
pub trait Matcher: Send + Sync {
    fn matches(&self, ctx: &mut MatchContext<'_>) -> Result<MatchResult, PolicyError>;
}
pub trait Action: Send + Sync {
    fn execute(&self, ctx: &mut RequestContext) -> Result<ActionResult, PolicyError>;
}
pub enum ErrorClass { LocalNotFound, LocalMethod, LocalUnavailable, UpstreamResponse,
    Authentication, InvalidInput, Budget, UpstreamTransport, Cancelled, Committed }
pub struct PolicyError { pub class: ErrorClass, pub status: u16, pub code: &'static str }
pub fn classify_error(error: &PolicyError, response_started: bool) -> ErrorClass;
pub fn compile(config: &crate::config::model::ValidatedConfig)
    -> crate::config::error::Result<Vec<Option<RuleChain>>>;
```

上述 context 和 ID 是 Rill 内部类型，不是 Pingora API；ID 只能索引同一快照内的已准备动作/鉴权/过滤器，不能回读活动配置。异步动作由执行器分派：Authorize 成功后继续当前规则，失败直接终止；Terminal 结束规则遍历。trait 的同步方法只产生控制决策，不阻塞执行 IO。`Vec` 与 sites 一一对应，旧模式为 None。matcher 编译、资源准备错误都携带源位置；新字段纳入既有 parse → normalize → validate，再由 `config::prepare::build`/`build_for_run`/`build_tracked` 中的 ResourcePreparer 调用 compile 和 prepare。不得从 CLI 另建路径。

```rust
// 既有 src/config/prepare.rs，扩展而不取代
pub trait ResourcePreparer {
    type Resources;
    fn prepare(&self, configuration: &ValidatedConfig) -> Result<Self::Resources>;
    fn prepare_tracked(&self, configuration: &ValidatedConfig,
        _dependencies: &mut DependencyPlan) -> Result<Self::Resources> {
        if !configuration.certificates.is_empty() {
            return Err(ConfigError::new("INTERNAL", "watch",
                "resource preparer does not support tracked TLS",
                &configuration.server.origin));
        }
        self.prepare(configuration)
    }
}
// PreparedSnapshot<R> 仍为 { configuration: ValidatedConfig, resources: R }。
// 新增准备结果；与 sites 同索引，旧模式为 None。
pub struct PreparedPolicy { pub chains: Vec<Option<RuleChain>> } // 手动 impl Debug
// src/runtime/state.rs：在现有 RuntimeSnapshot 增加字段：
// pub policy: PreparedPolicy,
// HttpResources 也携带 policy，候选构造时移入 RuntimeSnapshot，一次 ArcSwap 发布。
// HttpResources 与 RuntimeSnapshot 现有 derive(Debug) 保留；RuleChain/PreparedPolicy 因持有
// dyn Matcher/dyn Action 不能 derive，必须手写 Debug（只打印规则数、标识与错误链数量），
// 否则现有 derive 无法编译。
```

```rust
// src/webdav/mod.rs
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct StorageIdentity { pub storage_id: Box<str>, pub device: u64, pub inode: u64 }
pub struct StorageRegistry; // 进程所有；不是 RuntimeSnapshot 所有
pub struct StorageHandle;
pub struct StorageCandidate;
pub struct ActivatedStorage; // 未提交 Drop 只撤销自己的未发布 registry/属性候选
pub struct DavError;
impl StorageRegistry {
    pub fn prepare(&self, identity: StorageIdentity,
        root: std::sync::Arc<std::fs::File>) -> Result<StorageCandidate, DavError>;
}
impl StorageCandidate {
    pub fn activate(self) -> Result<ActivatedStorage, DavError>;
}
impl ActivatedStorage {
    pub fn handle(&self) -> &std::sync::Arc<StorageHandle>;
    pub fn commit(self); // 单快照 store 后无失败提交，不迁移/清空旧锁
}
```

现有 `ResourcePreparer` 的默认 `prepare_tracked` 在存在证书时返回 INTERNAL，否则委托 `prepare`；`ConfigurationOnly` 和 `HttpPreparer` 均覆盖该方法以跟踪依赖。扩展两者而非替换接口：check 的 `CheckResources` 携带经编译的 `PreparedPolicy` 并报告策略验证结果，不创建策略运行资源、日志文件或 worker；现有 TLS 和静态 root 检查仍照常准备，不声称 check 不打开任何资源。run 的 `HttpResources` 持有完整准备策略，按 site 索引移入快照。

锁由进程级 registry 以 storage-id 和准备时 root 的设备/ inode 身份拥有；同身份跨 reload 保锁，重启失锁。同 storage-id 的 root 身份变化返回 `RESTART_REQUIRED`，不得把旧锁转到新 root。准备不修改活动 registry；所有可失败的检查、配额、属性存储与候选激活均在快照 store 前完成，ActivatedStorage 以 RAII 持有未发布资源，其他资源激活失败或最终 gate 检查失败时 Drop 只回滚自己的未发布条目，不触碰旧锁/handle。完整候选持有这些 handle 后一次 store，随后 commit 不可失败；不得在 store 后执行返回 Result 的资源准备。激活与候选提交串行；旧请求持有原 handle。registry 同时持有跨代 namespace 锁：内部 PUT 提交、DELETE、MOVE、MKCOL、COPY 与属性变更遵守同一锁，覆盖父 FD 解析、身份复验及提交；临时上传需持有能阻止父目录 MOVE 的 namespace 保护，取消会清理同一父 FD 下本事务拥有的临时条目。命名空间锁不能替代 WebDAV 逻辑锁/token 验证；后者在相同临界区再次校验。历史身份、私有属性 inode、锁 owner、XML/遍历和输出信用复用既有进程 64 MiB policy pool，分配前预留；历史/旧代/真实任务不因短快照释放提前退券，属性删除、替换及锁过期回收对应额度。父代理确认的 syscall 边界：所有长耗时准备、临时 sync、父 FD/条件重核都在最终 commit 准入 CAS 前，紧邻准入检查取消和期限；CAS 不是 rename 的文件系统线性化证明。取消先赢则绝不 rename 并清理自己的 temp；有效期限内 commit 准入先赢后只剩无 await/无排队的不可抢占 rename/父 sync，可能跨过断开/期限，该次客户端结果未知，不声称回滚、不自动重试。rename 失败清 temp；成功后父 sync 或回复失败不能删除新 target 假装恢复旧文件。承袭现有静态 root 的信任边界：服务 UID/root/管理员对本地文件系统的并发变更是受信行为，不声称抵御恶意管理员在复验之后移动父目录；内部 DAV 操作不能绕过 namespace 锁。属性持久化独立于内存锁，原子性按 3.8。

```rust
// src/observability/owner.rs
pub struct LogCandidate;
pub struct ActivatedLogs; // 未提交时 Drop 撤销候选独占创建的新文件
pub struct LogSink;
pub struct LogRegistry; // 进程持有，按完整目录身份、文件名与打开文件身份索引
pub struct LogError;
pub enum PrepareMode { Check, Run }
impl LogCandidate {
    pub fn prepare(config: &[LogConfig], mode: PrepareMode,
        registry: &LogRegistry) -> Result<Self, LogError>;
    pub fn activate(self) -> Result<ActivatedLogs, LogError>;
}
impl ActivatedLogs {
    pub fn sinks(&self) -> &[std::sync::Arc<LogSink>];
    pub fn commit(self); // 发布后不可失败，解除候选回滚并唤醒已就绪 worker
}
pub struct LogConfig; // 经验证的 site 日志声明
```

check 仅验证配置、父目录安全与可观察权限，不创建文件、不轮转、不启动 worker；不能保证实际写入必成功。run 候选准备只打开并验证现有父目录 FD、预留有界配额，不创建或修改目标。激活不重命名、不截断、不轮转活动文件：同一安全目录/文件身份直接复用现有 Arc<LogSink> 与 worker；新目标以受约束 O_CREAT|O_EXCL 打开，并将创建所得身份记入回滚日志。未知既存普通目标用 O_APPEND|O_NOFOLLOW 安全打开；准备与激活复验需有界枚举本目标管理的全部 `file.N`（N 为正整数；1 最新，最多保留至 5）轮转集合，单父目录至多扫描 4096 项，超限拒绝 `LOG_LIMIT`。逐个拒绝链接/非普通文件，核对每文件不超过第 5 节绝对 10 MiB 上限、数量不超过 retain-files、总字节不超过站点/进程配额，超过保留期限的既存轮转也拒绝 `LOG_LIMIT`；不在失败候选中整理旧文件。写权限失败只拒绝候选，发布前不写入；同一候选重复目标或相互冲突的打开/轮转声明拒绝。每次实际写入前再次计入即将写入的完整有界记录，超过该记录所属快照的 rotate-size 则先按序轮转并回收超龄/超量轮转；rotate-size 是新记录的触发阈值，不承诺既存或旧代生成的轮转文件立即缩小到新阈值，但任何文件仍受 10 MiB、每站点 6 文件/60 MiB 的绝对上限约束。字段白名单和轮转声明随记录持有原请求快照，旧请求可以完成旧政策；无法在约束内轮转或写入时丢弃该条并记录限频诊断，不突破硬上限。外部同 UID/root 任意改写日志文件不在配额保证内。worker 与队列配额提前保留，所有 worker 就绪后仍停在候选门闩；任何激活失败撤销候选拥有的新文件，不删除既存目标。提交 gate 内先检查关闭/期限/启动项，再把持有 sinks 的完整快照一次 store；随后 commit 无失败唤醒 worker，store 与唤醒间的记录仅进入有界队列。旧 worker 不停止直至最后旧请求/快照持有者释放；停止/flush 由 registry 管理，不在业务请求 Drop 中无限 join。回滚仅在 dev/ino/type 与本候选记录一致时删除自己的新文件。该协议保证失败候选不污染活动资源，不承诺进程崩溃时多个文件系统路径整体原子回滚；不允许以多目标 rename 的假想原子性替代单快照提交。慢盘只丢日志并有界诊断。路径冲突、旧代持有导致的预算不足拒绝候选。

`src/auth/` 拥有验证配额与凭据；`src/body_transform/` 拥有流式变换及全局预算；`src/routing/` 只选择 site 和旧路由/新链；`src/proxy/` 负责协议、地址策略及流生命周期；`src/runtime/` 拥有进程 registry 和单发布点。源代码 `PeerOptions::set_http_version(max, min)` 存在；Pingora 0.9.0 的 h2 路径有 response trailers 处理和 trailer filter，h1 路径标记 trailers TODO。这些源码事实不证明完整双向 gRPC、跨协议 trailers 或取消可用；机制必须经 001-T03 实际隔离验证，失败阻断，不静默降级、发明 API 或更换框架。流式正则、DAV 事务和日志无副作用激活也受同一关卡约束。

### 6.1 上游池政策身份与 trailers 边界

2026-11-05 用户补充批准：候选准备冻结系统 CA 文件和 hash 目录材料；源内容后续变更须 Reload 才影响新快照。每个源目录最多 4096 原始目录项、单文件最多 1 MiB；完整材料计入现有候选策略 8 MiB、进程策略 64 MiB 预算，包含候选及旧代持有资源。允许安全跟随系统 hash 符号链接（包括绝对目标），最终目标须是稳定的普通文件；断链、不可读或读取期间变化拒绝候选。保留目录顺序、hash 后缀空洞及 CRL 查找语义；check 只读，run 持有私有副本。上游 h2 每连接最多 16 个并发流，尊重后端更低 SETTINGS；满流按框架常规建立新连接，不降级。连接出生记录另限全进程 4096，包含旧连接和活动流；准入前回收弱引用已失效的记录，满额明确拒绝，不降级。2026-11-05 用户另确认预算口径：8 MiB/候选和 64 MiB/进程覆盖 Rill 持有的 CA 原始材料、可计量副本及元数据；不包括 OpenSSL 内部解析临时内存、证书对象和 X509 store 惰性缓存，也不是进程 RSS 硬上限。后者仍受有界输入、身份及连接记录数和共享 store 生命周期约束。005 已按上述补充完成生产实现与隔离验收，来源、计量边界及证据见 [019](../../validation/019-caddy-http2-grpc.md)。

`PoolIdentity` 按完整协议、已批准目标 SocketAddr、TLS 开关、验证名、完整信任材料和连接选项精确比较。进程 registry 为不同身份 checked_add 分配唯一 u64 `PoolToken`，作为 HttpPeer.group_key 的输入；token 不从 64 位截断哈希推断身份。政策/请求持有同一 registry lease，Reload 复用相同身份，失败候选不修改任何已活动身份；无持有者的表项可回收，但已分配的 token 不重用。池本身仍由 Pingora 管理，不换框架。

Pingora 0.9.0 默认 HttpPeer::Hash 不涵盖 CA 内容与 ALPN，不能只设置 PeerOptions 后假定池隔离成立。group_key 也不是授权：connected_to_upstream 在发送业务 HTTP 字节前，对连接记录的完整政策 token 做精确复验，不匹配或缺失一律 `UPSTREAM_POLICY_MISMATCH` 拒绝。连接记录绑定连接出生身份与弱引用生命周期，不以裸 fd 整数当永久身份；关闭清除，fd 重新分配属于新连接。未知连接先拒绝而不是猜测可复用，内部额度包含尚未结束的旧请求。原型用有限 fd→token 表验证该挂钩和强制哈希键碰撞下的 fail-closed 行为；005 落地必须补齐连接出生/回收及并发准入，不能原样移植无界表。

```rust
// src/proxy/pool_policy.rs
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolIdentity {
    pub protocol: UpstreamProtocol,
    pub target: std::net::SocketAddr,
    pub tls_name: Option<Box<str>>,
    pub trust_der: Vec<Box<[u8]>>,
    pub connection_options: Box<[u8]>, // 规范化的全部影响连接身份/验证的选项
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamProtocol { Http1, H2c, TlsH2 }
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PoolToken(pub u64);
pub struct PoolLease; // 持有完整 identity 和 token
pub struct PoolRegistry;
impl PoolRegistry {
    pub fn prepare(&self, identity: PoolIdentity) -> crate::config::error::Result<std::sync::Arc<PoolLease>>;
    pub fn verify_reused(&self, lease: &PoolLease,
        connection: &ConnectionIdentity) -> Result<(), crate::http_policy::PolicyError>;
}
pub struct ConnectionIdentity; // 活连接出生身份，不可仅为 RawFd
```

本计划的完整 gRPC 包括四类 Length-Prefixed-Message 调用及响应 trailers/trailers-only；请求消息结束是 EOS，不能和通用 HTTP 请求 trailers 混为一谈。先前记录把“请求 trailers 尚未验证”加为独立必需项，没有原始需求依据，现纠正为能力边界：Pingora 0.9.0 公开请求读取/转发路径未暴露对应 trailers 接口，不宣称透传通用 HTTP 请求 trailers；响应 grpc-status/grpc-message/custom metadata 已由容器实测。HTTP/1 framing 与 trailers 的限制保持显式，不能把 HTTP/2 gRPC 静默降级到 HTTP/1。

## 7. 错误与失败语义

| 条件 | 状态 | handle-errors 可匹配 | 不可吞掉 |
| --- | --- | --- | --- |
| 本地文件缺失、方法不支持 | 404、405 | 本地 `error-status` | 非 Missing 的路径/权限错误 |
| 本地资源临时不可用 | 503 | 本地 `error-status`，只能保持错误状态 | 安全及容量保护 |
| 上游正常 HTTP 响应，包括 404 | 原 200–599 | 否，普通响应条件处理 | 不把上游 404 当本地错误 |
| 鉴权失败 | 401 | 否 | challenge、认证失败 |
| 非法路径、目标、拒绝地址 | 400/403 | 否 | 安全拒绝 |
| 预算/容量、DAV 超限 | 503，DAV 大小超限 413 | 否 | 超限事实 |
| 上游连接/读取错误 | 502/504 | 否 | 传输错误及超时 |
| 客户端取消或已发送响应头 | 不再生成状态 | 否 | 取消、流失败 |

错误链不得将错误改写成 2xx/3xx；不得递归进入。流式正则与压缩在发送头之前超预算返回 503，发送后取消流/关闭对应连接并记失败，不能补发状态或记录成功。无界重复可等到 EOF；待决到限中止，不使用固定重叠窗口假装完整匹配。解码、替换及编码每步计入全局和请求预算。失败候选从不修改活动资源，旧快照继续服务；任何不能保证该性质的机制阻断后续阶段。

## 8. 依赖与工具

以下状态逐项对照现有 Cargo.lock；在锁文件中不代表是直接依赖，使用时仍需增加精确直接依赖及 feature。`zstd` 锁有 0.13.4 和 0.14.3，直接依赖拟选 `=0.13.4`。许可证列来自本地 registry 解包源码中对应版本的 Cargo.toml 元数据，仍须正式复核。

| crate | 精确版本 | 状态 | 用途、feature | 许可证 |
| --- | --- | --- | --- | --- |
| regex | =1.13.1 | 001 已作为 dev-dependency 直接引入，生产用途仍需直接依赖条目 | 规则/正文正则，默认 Unicode | MIT OR Apache-2.0 |
| regex-syntax | =0.8.11 | 001 已作为 dev-dependency 直接引入；原已在 lock 中 | HIR maximum_len 检查，默认 | MIT OR Apache-2.0 |
| h2 | =0.4.20 | 在 lock 中；001 已作为 dev-dependency 直接引入，生产用途仍需新增直接依赖条目 | gRPC 测试；h2c/TLS h2 四类调用原型容器通过，生产集成仍由 005 实现；池政策原型及强制碰撞已验证 | MIT |
| zstd | =0.13.4 | 在 lock 中，需添加直接依赖条目 | 编码，默认 | BSD-3-Clause |
| flate2 | =1.1.10 | 在 lock 中，需添加直接依赖条目 | gzip，默认 Rust 后端 | MIT OR Apache-2.0 |
| futures | =0.3.34 | 在 lock 中，需添加直接依赖条目 | 有界流组合，默认 | MIT OR Apache-2.0 |
| base64 | =0.22.1 | 在 lock 中，需添加直接依赖条目 | Basic 解码，默认 | MIT OR Apache-2.0 |
| percent-encoding | =2.3.2 | 在 lock 中，需添加直接依赖条目 | URL 编码，默认 | MIT OR Apache-2.0 |
| tokio-stream | =0.1.19 | 在 lock 中，需添加直接依赖条目 | 必要流适配，默认 | MIT |
| http | =1.5.0 | 在 lock 中；001 已作为 dev-dependency 直接引入，生产用途仍需新增直接依赖条目 | HTTP 类型，默认 | MIT OR Apache-2.0 |
| argon2 | =0.5.3 | 不在 lock 中，新增包且需修改 lock | Argon2id，std/password-hash | MIT OR Apache-2.0 |
| bcrypt | =0.17.0 | 不在 lock 中，新增包且需修改 lock | bcrypt 验证，默认 | MIT |
| globset | =0.4.16 | 不在 lock 中，新增包且需修改 lock | 路径 glob，默认 | Unlicense OR MIT |
| url | =2.5.4 | 不在 lock 中，新增包且需修改 lock | 动态 URL 规范化，默认 | MIT OR Apache-2.0 |
| quick-xml | =0.38.4 | 不在 lock 中，新增包且需修改 lock | 有界 DAV XML，默认，禁外部实体 | MIT |
| hyper | =1.6.0 | 不在 lock 中，新增包且需修改 lock | 仅 dev-dependency；隔离 HTTP/gRPC 测试，client/server/http1/http2；可行性待 001-T03 | MIT |
| prost | =0.13.5 | 不在 lock 中，新增包且需修改 lock | 仅 dev-dependency；测试消息，默认 | Apache-2.0 |

001 主工程 dev-dependencies 为 h2 0.4.20、http 1.5.0、regex 1.13.1、regex-syntax 0.8.11，四者本已在 Cargo.lock 中，锁文件仅增加 rill 包对应依赖名，无新包或版本变化。容器工具使用独立工作区/锁文件，tonic/prost 等不进入生产依赖；确切来源/版本见 014/015。已检查本地 cargo 缓存：quick-xml 0.38.4 的离线包可用，但仅在本地缓存而不在 Cargo.lock；加入会修改 Cargo.lock 及其传递依赖树。离线可用不构成依赖批准，新增依赖仍需记录批准。

新增依赖需分别复核版本、feature、许可证及传递树；缓存检查不构成构建或可行性验证。锁文件中不存在的条目不得称作已锁定。精确 pin，使用 `--locked`，不得 `cargo update`；网络获取 crate、客户端工具或其他工件须另行批准。rclone/cadaver 仅用于 008 隔离验收，记录实际版本，不作为运行时依赖；不在本文工作中安装或运行。

## 9. 用户验收清单

用户在收到最终契约、015 证据和资源边界说明后明确回复“验收通过”，确认接受下列全部条目：

- [x] 接受第 2 节范围与排除项，以及第 3–4 节 DSL、受保护头、响应条件和执行语义。
- [x] 接受新增配置上限：index 每动作 1 个、1024 字节；try-files 每动作 64 项、每路径 4096 字节；access-log 每站点 1 个、每候选 64 个；每站点主链及错误链 4096 条规则；每候选编译策略 8 MiB、使用链的站点 128 个；超限按第 5 节拒绝。
- [x] 接受策略限额：每请求 1024 步、嵌套 16、每链 matcher 256、正则 4096 字节及重复 1000、程序和 DFA 各 2 MiB、glob 1024 字节、重入 4 次、每响应替换 16 条、主链和错误链合计 encode 最多 1 个。
- [x] 接受正文限额：每请求缓冲 1 MiB、全进程 64 MiB；解码 64 MiB、100 倍加 64 KiB；输出 64 MiB、CPU 完成预算及执行等待各 2 秒（不可抢占单次库调用，超时任务保留额度）、并发 64；gzip level 3/window 32 KiB、zstd level 3/window 1 MiB。
- [x] 接受鉴权限额：bcrypt cost 10–14；Argon2id v19、8192–65536 KiB、迭代 1–3、并行度 1–2；4 个任务、2 秒、不排队；令牌文件与 Basic 输入各 4096 字节。
- [x] 接受 DAV 限额：锁每存储 1024、全进程 4096、3600 秒；属性每资源 64 项/64 KiB，XML 1 MiB/深度 32；遍历 4096 项/深度 16；临时 PUT 每文件 256 MiB、全进程 1 GiB、并发 16、300 秒。
- [x] 接受动态出口限额：URL 4096 字节、1 跳、不跟随重定向；活动请求 128、DNS 候选 64、每 origin 存活物理连接 8（含握手与空闲连接；2026-11-06 澄清）。
- [x] 接受日志限额：队列每站点 1024、全进程 16384、每条 16 KiB；站点/sink/worker 各 64、FD 总计 256，均含候选和旧代；文件 10 MiB、轮转保留 5 个/7 天，每站点 6 文件/60 MiB、全进程 384 文件/3840 MiB，关闭等待 5 秒。
- [x] 接受第 5 节其余既有限额：DNS 60 秒/失败退避 1 秒/64 候选/8 尝试；代理期限 5/60/300/60 秒；入口 60 秒/64 KiB/1024 槽；TLS/H2 10 秒/10 秒/100 流/64 KiB/60 秒；静态缓存 0 默认/256 MiB 最大/64 KiB 对象/1024 项及静态 IO 128 槽；加载 1 MiB 每文件/8 MiB 总量/512 文件/导入深度 64/KDL 深度 32；snippet 深度 64/4096 路由/16384 步/8 MiB；错误页 1 MiB、列表扫描 4096/展示 512/HTML 256 KiB；控制帧 64 KiB/首字节后 5 秒/reload 默认 30 秒。Pingora h2 可行性证据见 014/015。
- [x] 接受第 6 节接口、单快照发布、DAV 跨代锁及日志候选事务边界。
- [x] 接受第 8 节各依赖、feature、许可证和必要客户端；任何网络获取须单独批准。
- [ ] 接受或拒绝 [015 收口记录](../../validation/015-caddy-contracts-feasibility-closure.md)：完整 gRPC 原型、响应 trailers、同进程政策池隔离与强制键碰撞拒绝、无界捕获正则 216 分块、DAV/日志跨快照资源机制均已实测；技术任务与独立复审通过。用户明确验收前 T04/AC02 保持未勾选，不创建阶段完成提交、不进入 002。
