# 基于 Pingora 的轻量级 Web 服务器需求文档

**版本：** v0.1.0  
**状态：** 初始需求定义  
**技术语言：** Rust  
**核心框架：** Cloudflare Pingora  
**配置语言：** KDL  
**目标平台：** Linux（优先 Debian）

---

## 1. 项目概述

### 1.1 项目定位

开发一个基于 Rust 和 Cloudflare Pingora 的轻量级、高性能 Web 服务器，主要用于替代 Nginx、Caddy 的常用功能。

项目专注于：

- HTTP / HTTPS 反向代理
- 静态文件托管
- 多域名虚拟主机
- 动态路由更新
- 静态 TLS 证书管理
- 无中断配置重载
- 简洁且可复用的配置系统

不追求完整复刻 Nginx 或 Caddy，而是通过最小必要功能实现稳定、高性能、低资源占用的服务。

### 1.2 核心设计原则

1. **简单优先**：减少不必要的配置项、运行时组件和依赖。
2. **性能优先**：利用 Pingora 的连接复用、异步 I/O 和代理能力。
3. **动态更新**：修改域名、路由、上游服务器及证书时，无需重新启动服务。
4. **配置可读**：使用 KDL 配置语言，缩进不影响语义。
5. **配置可复用**：支持多域名绑定、公共配置继承和多文件导入。
6. **安全可靠**：所有配置更新必须经过完整验证，失败时保留原有配置。
7. **易于部署**：单二进制运行，支持 systemd 和 Podman 部署。

---

## 2. 技术选型

| 模块 | 推荐技术 |
|---|---|
| 开发语言 | Rust Stable |
| 网络代理框架 | Pingora |
| 异步运行时 | Tokio |
| TLS | OpenSSL（复用 Pingora TLS 能力） |
| 配置语言 | KDL |
| KDL 解析 | kdl crate |
| 配置反序列化 | 自定义解析 + Serde |
| 动态配置 | ArcSwap |
| 路由匹配 | 自定义 Host 匹配 + matchit 或等效实现 |
| 静态文件 | Tokio 文件 I/O |
| 文件变化监测 | notify（可选） |
| 日志 | tracing |
| 指标 | Prometheus（可选） |

必须优先复用 Pingora 已经提供的网络能力，避免重新实现 HTTP 协议解析、连接管理、TLS 握手等底层功能。

---

## 3. 核心功能需求

### 3.1 HTTP / HTTPS 服务

**优先级：P0**

要求：

- 支持 HTTP/1.1。
- 支持 HTTP/2。
- 支持 IPv4 和 IPv6。
- 支持自定义 HTTP 和 HTTPS 监听地址及端口。
- 支持多域名共用监听端口。
- 支持 HTTP 到 HTTPS 重定向。
- 支持 HTTP Keep-Alive 和连接复用。
- 支持优雅关闭及连接排空。

HTTP/3 暂不作为首期开发目标。

### 3.2 反向代理

**优先级：P0**

支持通过域名和请求路径将流量转发到指定上游。

基础配置：

```kdl
site "api.example.com" {
    proxy "http://127.0.0.1:8080"
}
```

等价于：

```kdl
site "api.example.com" {
    route "/*" {
        proxy "http://127.0.0.1:8080"
    }
}
```

必须支持：

- HTTP / HTTPS 上游。
- 域名匹配。
- 路径匹配。
- 自定义上游端口。
- HTTP/1.1、HTTP/2 客户端请求。
- WebSocket Upgrade。
- SSE 流式响应。
- 请求体、响应体流式转发。
- 上游连接池与连接复用。
- 请求超时和连接超时。
- 上游异常时返回正确的 HTTP 错误。
- 保留并正确处理 Host、Forwarded 和 `X-Forwarded-*` 等请求头。

路径转发默认保留原始请求路径，不进行隐式重写。

### 3.3 多域名支持

**优先级：P0**

一个 `site` 配置块必须允许绑定多个域名：

```kdl
site "example.com" "www.example.com" "app.example.com" {
    tls "default"
    proxy "http://127.0.0.1:8080"
}
```

以上域名共享同一组代理、静态文件和 TLS 配置。

支持精确域名：

```kdl
site "api.example.com" "admin.example.com" {
    proxy "http://127.0.0.1:3000"
}
```

支持通配符域名：

```kdl
site "*.example.com" {
    proxy "http://127.0.0.1:8080"
}
```

域名匹配规则：

1. 精确域名优先于通配符域名。
2. 通配符只匹配明确规定的子域名层级。
3. 域名匹配不区分 ASCII 大小写。
4. 禁止重复或存在歧义的域名配置。
5. 未匹配的域名默认拒绝服务或返回 404，不能意外落入其他站点。

### 3.4 路由匹配

**优先级：P0**

支持一个站点配置多个路由：

```kdl
site "app.example.com" {
    route "/api/*" {
        proxy "http://127.0.0.1:8080"
    }

    route "/assets/*" {
        root "/srv/www/assets"
    }

    route "/*" {
        root "/srv/www/app"
        spa true
    }
}
```

路由规则：

- 精确匹配优先于前缀匹配。
- 更长的有效前缀优先。
- `/*` 作为站点兜底路由。
- 配置块声明顺序不应影响确定性的路由结果。
- 不允许产生无法判定优先级的冲突。
- 支持在 Reload 时新增、修改、删除路由。

首期无需支持正则表达式路由。

### 3.5 静态文件托管

**优先级：P0**

直接托管 HTML、CSS、JavaScript、图片、字体等静态资源。

示例：

```kdl
site "www.example.com" {
    root "/var/www/example"
}
```

SPA 模式：

```kdl
site "app.example.com" {
    root "/var/www/app/dist"
    spa true
}
```

必须支持：

- 正确识别 MIME 类型。
- GET / HEAD 请求。
- Content-Length。
- ETag 或 Last-Modified。
- 条件请求与 304 响应。
- Range 请求与 206 响应。
- Cache-Control 配置。
- 目录索引文件，例如 `index.html`。
- SPA History 路由回退。
- 禁止目录穿越。
- 禁止访问站点根目录之外的文件。

SPA 回退只适用于符合条件的页面导航请求，不应将不存在的 JS、CSS、图片资源错误返回为 HTML。

可选增强：

- Gzip / Brotli 预压缩文件。
- 自定义错误页面。
- 静态文件缓存。
- 目录浏览（默认禁用）。

### 3.6 TLS 证书管理

**优先级：P0**

项目不需要自动申请或续期 TLS 证书。

证书由用户自行生成或通过其他工具提供。

要求：

- 支持 PEM 格式证书链。
- 支持 PEM 格式私钥。
- 支持多个域名复用同一证书。
- 支持不同站点绑定不同证书。
- 支持基于 SNI 选择证书。
- 支持通过 Reload 更新证书。
- 不需要 ACME、Let's Encrypt 或 DNS-01。

证书定义：

```kdl
tls "default" {
    cert "/etc/proxy/certs/fullchain.pem"
    key "/etc/proxy/certs/privkey.pem"
}
```

引用：

```kdl
site "example.com" "www.example.com" {
    tls "default"
    proxy "http://127.0.0.1:8080"
}
```

Reload 时必须：

1. 重新读取证书和私钥。
2. 校验证书与私钥是否匹配。
3. 校验证书有效期和域名覆盖范围。
4. 检查 TLS 上下文能否成功构建。
5. 全部验证通过后，替换当前证书配置。
6. 如失败，保留旧证书配置。

期望新 TLS 握手立即使用新证书，已建立连接保持正常运行。

TLS 无中断热更新需要通过 Pingora 和 OpenSSL 的扩展能力实现，不依赖自动证书管理模块。

### 3.7 动态路由及配置 Reload

**优先级：P0**

支持在不停止进程的情况下更新：

- 域名与站点。
- 路由规则。
- 上游服务器地址。
- 静态文件根目录。
- 代理超时等运行参数。
- TLS 证书与私钥。

推荐采用 `ArcSwap` 维护不可变运行时配置快照。

加载流程：

```text
读取 KDL 文件
      ↓
处理 import 和 profile
      ↓
语法解析
      ↓
配置语义校验
      ↓
构建路由表与 TLS 上下文
      ↓
完整验证
      ↓
原子替换运行时配置
      ↓
新请求使用新配置
```

Reload 必须具备事务性：

- 所有配置同时生效。
- 任何一处失败则整体回滚。
- 不允许出现部分站点更新、部分站点失败的状态。
- 不影响已有正常请求。
- 不主动中断已建立的 WebSocket 连接。
- 报错信息必须指明具体文件、配置项及位置。

监听端口、网络监听器等无法进程内安全修改的底层配置，应明确要求进程重启，不能假装热更新成功。

### 3.8 Reload 操作方式

**优先级：P0**

至少支持：

```bash
proxy-server check
proxy-server reload
```

以及：

```bash
proxy-server run --config /etc/proxy/config.kdl
```

其中 `proxy-server` 为暂定可执行文件名。

`check`：

- 验证语法及配置语义。
- 验证所有导入文件。
- 检查路由冲突。
- 检查证书和私钥。
- 不修改正在运行的配置。

`reload`：

- 向正在运行的服务发送 Reload 请求。
- 等待服务返回实际更新结果。
- 失败时返回非零退出码和错误原因。
- 不能仅表示 Reload 指令已发送成功。

推荐通过 Unix Domain Socket 或约定的 Unix 信号进行进程间通信。

文件变更自动 Reload 可作为 P1 功能。

---

## 4. KDL 配置系统

### 4.1 设计目标

**优先级：P0**

配置系统必须满足：

- 缩进仅用于视觉排版，不影响语义。
- 使用 `{}` 表示嵌套结构。
- 支持注释。
- 支持多文件配置。
- 支持公共参数复用。
- 支持多个域名共享配置。
- 支持简写语法。
- 未知配置项必须报错。
- 配置错误必须提供明确的位置和原因。

配置层应与 Pingora 的运行时实现解耦。

### 4.2 文件导入

支持导入单个文件和通配符路径：

```kdl
import "tls.kdl"
import "profiles/*.kdl"
import "sites/*.kdl"
```

建议目录结构：

```text
/etc/proxy/
├── config.kdl
├── tls.kdl
├── profiles/
│   └── default.kdl
├── sites/
│   ├── app.kdl
│   ├── api.kdl
│   └── grafana.kdl
└── certs/
    ├── fullchain.pem
    └── privkey.pem
```

要求：

- 相对路径基于当前配置文件所在目录解析。
- 通配符文件按确定顺序加载。
- 支持嵌套导入。
- 检测循环导入。
- 检测重复定义。
- 任意导入文件错误时，整体 Reload 失败。

### 4.3 Profile 配置复用

支持预定义公共参数：

```kdl
profile "backend" {
    connect-timeout "5s"
    read-timeout "60s"
    websocket true
}
```

站点或路由中引用：

```kdl
site "api.example.com" {
    use "backend"
    proxy "http://127.0.0.1:8080"
}
```

支持局部覆盖：

```kdl
site "slow.example.com" {
    use "backend"
    read-timeout "300s"
    proxy "http://127.0.0.1:9000"
}
```

合并规则：

- 后定义的本地显式参数覆盖 Profile 默认值。
- 不允许循环引用。
- 不允许引用不存在的 Profile。
- 冲突字段必须具有确定的优先级。
- 未被允许复用的参数应报错，而不是静默忽略。

### 4.4 可复用配置片段

**优先级：P1**

除参数 Profile 外，支持可复用的完整路由片段，避免多个站点重复声明相同规则。

建议设计 `snippet` 和 `use` 机制。需要明确区分 Profile 参数继承与路由片段展开，避免命名冲突和不确定合并。

### 4.5 配置简写

支持直接在站点中声明代理：

```kdl
site "api.example.com" {
    proxy "http://127.0.0.1:8080"
}
```

等价于：

```kdl
site "api.example.com" {
    route "/*" {
        proxy "http://127.0.0.1:8080"
    }
}
```

静态文件简写同理。

同一作用域如果同时存在相互冲突的简写和显式路由，应在配置校验阶段报告错误。

---

## 5. 完整配置示例

```kdl
// 全局监听配置
server {
    http "0.0.0.0:80"
    https "0.0.0.0:443"
    redirect-https true
}

// 导入其他配置
import "sites/*.kdl"

// TLS 证书
tls "default" {
    cert "/etc/proxy/certs/fullchain.pem"
    key "/etc/proxy/certs/privkey.pem"
}

// 公共代理参数
profile "backend" {
    connect-timeout "5s"
    read-timeout "60s"
}

// 多域名静态站点
site "example.com" "www.example.com" {
    tls "default"

    route "/api/*" {
        use "backend"
        proxy "http://127.0.0.1:8080"
    }

    route "/*" {
        root "/srv/www/example"
        spa true
    }
}

// 独立 API 服务
site "api.example.com" {
    tls "default"
    use "backend"
    proxy "http://127.0.0.1:3000"
}

// 通配符域名
site "*.internal.example.com" {
    tls "default"
    proxy "http://127.0.0.1:9000"
}
```

这是本项目期望实现的 KDL DSL 示例，不是 Pingora 的原生配置格式。

---

## 6. 安全要求

**优先级：P0**

### 6.1 静态文件安全

- 禁止 `../` 路径穿越。
- 防止 URL 编码绕过。
- 明确规范化路径的处理顺序。
- 防止符号链接越过允许的站点目录。
- 默认禁止目录列表。
- 不允许通过异常路径读取配置文件、密钥或系统文件。

### 6.2 反向代理安全

- 防止 Host Header 混淆。
- 防止 HTTP 请求走私。
- 正确处理 Hop-by-Hop Headers。
- 正确清理或更新来自不可信客户端的 Forwarded 头。
- 拒绝不合法的上游 URL。
- 限制管理配置能够指向的目标范围，避免非预期 SSRF。
- 防止请求和响应缓冲无限增长。
- 设置合理的连接、请求和空闲超时。

### 6.3 管理安全

- Reload 接口不得默认公开在互联网。
- 控制 Socket 使用最小权限。
- TLS 私钥不得写入日志。
- 配置文件和证书应采用最小访问权限。
- Reload 失败不得破坏运行中的服务。

---

## 7. 性能与资源要求

**优先级：P0**

性能目标：

- 低额外请求延迟。
- 低空闲内存占用。
- 高并发连接处理能力。
- 尽可能实现流式转发和零额外缓冲。
- 避免每请求执行文件系统配置读取。
- 避免全局互斥锁成为代理热路径瓶颈。
- Reload 不应造成明显的请求延迟尖峰。

不预先设定未经测试的 RPS 数值。

需要在同一环境下与 Nginx、Caddy 或 Pingap 进行基准对比。

测试项目：

| 测试场景 | 指标 |
|---|---|
| HTTP 反向代理 | RPS、p50/p95/p99 |
| HTTPS 反向代理 | RPS、CPU 使用量 |
| 静态小文件 | RPS、吞吐量 |
| 静态大文件 | 吞吐量、内存峰值 |
| WebSocket 长连接 | 连接数、RSS、断连率 |
| SSE 流式响应 | 首字节延迟、持续传输稳定性 |
| Reload 期间 | 错误率、连接中断数、延迟峰值 |
| 空闲状态 | RSS、CPU |

性能优化必须基于 Profile、压测或性能分析工具获得的数据，不进行无依据的优化。

---

## 8. 建议项目架构

```text
src/
├── main.rs
├── cli/
│   ├── mod.rs
│   ├── check.rs
│   └── reload.rs
├── config/
│   ├── mod.rs
│   ├── parser.rs
│   ├── loader.rs
│   ├── profile.rs
│   └── validate.rs
├── runtime/
│   ├── mod.rs
│   ├── state.rs
│   └── reload.rs
├── proxy/
│   ├── mod.rs
│   ├── handler.rs
│   ├── upstream.rs
│   └── routing.rs
├── static_files/
│   ├── mod.rs
│   ├── handler.rs
│   └── cache.rs
├── tls/
│   ├── mod.rs
│   ├── cert.rs
│   └── resolver.rs
└── observability/
    ├── mod.rs
    └── logging.rs
```

推荐职责边界：

- `config`：只负责解析、校验和构建标准化配置。
- `runtime`：维护当前运行时快照与 Reload 生命周期。
- `proxy`：负责转发和上游通信。
- `static_files`：负责文件响应和缓存。
- `tls`：负责证书加载与 SNI 解析。
- `cli`：负责命令行交互。
- `observability`：负责日志和运行指标。

模块不应直接互相操作内部状态，而应通过明确接口交互。

---

## 9. 日志与可观测性

### P0

- 结构化日志。
- 支持设置日志级别。
- 启动和停止日志。
- Reload 成功和失败日志。
- TLS 加载错误日志。
- 上游连接错误日志。
- 关键配置错误的文件名和位置。

### P1

- HTTP Access Log。
- Prometheus Metrics。
- 上游连接状态统计。
- 请求量、状态码分布。
- 代理延迟统计。
- 活跃连接统计。
- OpenTelemetry 接入。

---

## 10. 非目标功能

以下功能不纳入首期开发范围：

- ACME 证书申请与自动续期。
- 图形管理界面。
- WAF。
- HTTP/3。
- Lua / WASM 插件系统。
- 复杂负载均衡算法。
- 服务发现。
- Kubernetes Ingress Controller。
- API Gateway 身份验证系统。
- 分布式配置存储。
- 每请求调用外部 API 选择代理目标。
- 全面的 Nginx 配置兼容。
- 完整 CDN 或边缘缓存系统。

这些能力后续按实际需求独立评估，避免功能膨胀。

---

## 11. 开发阶段

### 第一阶段：最小可用版本（MVP）

- Pingora HTTP / HTTPS 服务。
- KDL 基本解析。
- 单域名与多域名绑定。
- 基础路径匹配。
- HTTP 反向代理。
- 静态文件服务。
- 手动提供 TLS 证书。
- `check` 配置验证命令。
- 基础集成测试。

验收标准：可以用一个二进制替代普通 Nginx/Caddy 的基础代理及静态文件部署。

### 第二阶段：动态配置

- 完整配置验证。
- ArcSwap 原子路由更新。
- 运行时证书替换。
- `reload` 命令。
- 多文件导入。
- Profile 参数复用。
- Reload 失败回滚。
- WebSocket 连接持续性测试。

验收标准：修改站点、路由、上游或证书后执行 Reload，无须重启服务，并且不主动中断现有连接。

### 第三阶段：生产增强

- 静态文件缓存优化。
- 完整缓存头支持。
- 请求及上游超时管理。
- SSE / WebSocket 压测。
- 安全审计。
- Prometheus Metrics。
- Graceful Shutdown。
- systemd 和 Podman 部署示例。
- 与 Nginx / Caddy 的性能对比。

验收标准：具备基础生产部署和故障排查能力。

---

## 12. 验收测试

| 测试编号 | 测试内容 | 期望结果 |
|---|---|---|
| T01 | 同一 site 绑定三个域名 | 三个域名使用相同路由 |
| T02 | 精确域名与通配符域名重叠 | 精确域名优先 |
| T03 | `/api/*` 与 `/*` 同时存在 | 正确匹配目标路由 |
| T04 | 修改上游并执行 reload | 新请求转发到新上游 |
| T05 | Reload 配置包含语法错误 | 保留旧配置 |
| T06 | Reload 证书不合法 | 旧证书继续生效 |
| T07 | 替换有效证书后 Reload | 新 TLS 握手使用新证书 |
| T08 | Reload 期间保持 WebSocket | 原有连接不被主动断开 |
| T09 | Vue History 模式直接刷新子路由 | 正确返回 index.html |
| T10 | 请求不存在的 JS 文件 | 返回 404，而不是 HTML |
| T11 | 尝试通过 `../` 访问其他目录 | 拒绝访问 |
| T12 | 修改导入文件中的 Profile | Reload 后按预期生效 |
| T13 | 导入文件存在循环引用 | 配置校验失败 |
| T14 | 定义重复或冲突路由 | 配置校验失败 |
| T15 | SSE 长时间流式响应 | 不被无故缓冲或截断 |
| T16 | 大文件 Range 请求 | 正确返回 206 和指定内容 |
| T17 | 在并发流量下反复 Reload | 无配置半更新、无异常崩溃 |
| T18 | 未配置的 Host 请求 | 不泄露其他站点内容 |

必须针对配置解析、路由匹配、TLS 更新和静态文件路径安全编写单元测试与集成测试。

---

## 13. 最终交付要求

交付内容：

1. 可编译、可运行的 Rust 项目。
2. 完整 Cargo 工程及依赖锁定文件。
3. KDL 配置格式说明。
4. 示例配置。
5. CLI 使用说明。
6. systemd Service 示例。
7. Podman 部署示例。
8. 自动化测试。
9. 性能测试脚本。
10. README 和架构说明。

代码要求：

- 使用 Rust Stable。
- 执行 `cargo fmt`。
- 通过 `cargo clippy`。
- 避免不必要的 `unsafe`。
- 关键接口具备单元测试。
- 不使用 `unwrap()` 处理可恢复的外部错误。
- 保持模块边界清晰。
- 新增功能不得破坏现有配置兼容性，确需变更时必须明确迁移方式。

---

## 14. 最终目标

构建一个具备以下特点的 Rust Web 服务器：

**单二进制、低资源占用、高性能反向代理、原生静态文件托管、多域名绑定、简洁的 KDL 配置、可靠的配置复用、静态 TLS 证书以及无中断 Reload。**

首要目标是实现 Nginx 和 Caddy 最常使用的核心功能，而非复刻其全部生态。

最终产品应当做到：

- 简单场景只需要几行配置。
- 复杂场景可以通过多个 KDL 文件组织和复用。
- 修改代理规则时不需要重启进程。
- 替换证书后执行 Reload 即可生效。
- 配置错误不会影响当前服务。
- 能在普通 Linux 服务器上长期稳定运行。
