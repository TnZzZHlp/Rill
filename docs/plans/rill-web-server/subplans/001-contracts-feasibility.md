# 001：配置契约与技术可行性

## 元数据

- 总计划：[Rill Web 服务器实施计划](../README.md)
- 阶段 ID：001
- 实施状态：已验收；用户已确认契约及首次提交范围，阶段关闭以本次独立完成提交成功为准
- 依赖：无
- 目标引用：G01
- 决策引用：总计划 D01–D08，具体契约以 001 交付并确认的版本为准。

## 目标与范围

冻结跨模块契约，验证 Pingora 的关键扩展点，避免在实现末期发现无法满足原子 Reload。

包含本阶段任务明确列出的能力。排除其他阶段的实现及原文非目标功能；不得借本阶段扩大范围。

## 前置条件

已获得独立实施授权，具备 Rust Stable、Debian/Linux 与 OpenSSL 验证环境。
实施前阅读目标文件与工作区状态，保留现有用户编辑。验证需要真实配置、证书或上游时，在受控环境准备，不使用真实私钥作为仓库内容；新增依赖或测试工具须遵守批准边界。

## 设计与模块边界

- 本阶段目标路径：docs/configuration.md、docs/architecture.md、docs/validation/001-feasibility.md、tests/feasibility.rs、tests/feasibility/、Cargo.toml、Cargo.lock。生产 config/routing/runtime/tls 模块在后续阶段实现；001 只形成契约和隔离的技术验证。
- 配置模型与 Pingora 类型解耦。拟议接口为 load → normalize → validate → prepare → publish；只有 publish 修改活动状态。RuntimeSnapshot 包含配置代次、路由、代理策略、静态根目录句柄及 TLS 材料。监听设置独立作为启动配置。
- main/CLI 仅编排，不持有业务规则；模块通过明确接口通信，不直接改写其他模块内部状态。
- 外部错误必须可恢复、可定位并脱敏。所有配置更新复用统一验证与准备流程，禁止绕过运行时事务边界。

## 实施任务

- [x] **001-T01：确定配置和 CLI 语义**
  - 依赖：无。
  - 目标路径：docs/configuration.md、docs/architecture.md。
  - 行动：明确 KDL 版本及布尔值示例兼容性、最终二进制名与 run/check/reload 参数；定义域名规范化、通配符层级、无 Host/未知 Host 行为、精确及前缀路径边界、路径编码和 root 映射。为相对 root/cert/key 路径明确基准，不默认套用 import 规则。
  - 预期交付物：配置语义决策表和合法/非法示例。
  - 验收引用：001-AC01。
  - 计划验证：对原文每个示例逐项评审，明确修正或兼容行为。
  - 完成证据：docs/configuration.md 第 1–8 节逐项确定语义与原文示例迁移；配置语法验证通过，用户已确认契约。详见 docs/validation/001-feasibility.md。

- [x] **001-T02：确定继承、安全与管理契约**
  - 依赖：001-T01。
  - 目标路径：docs/configuration.md、docs/architecture.md。
  - 行动：定义 Profile 允许字段、多 Profile 优先级、站点到路由继承、局部覆盖和简写冲突规则；区分 profile 与 snippet 名称空间。明确 Host/SNI 策略、可信转发代理、上游 Host、HTTPS 上游验证和访问目标允许范围，不能一律禁用文档所需内网后端。确定 Socket 路径、权限、调用者授权与响应协议。
  - 预期交付物：模块接口、信任边界和错误模型。
  - 验收引用：001-AC02。
  - 计划验证：检查所有歧义均有确定规则；不存在默认信任客户端 Forwarded 的行为。
  - 完成证据：docs/architecture.md 第 2–8 节明确接口、信任边界和错误分类；复审通过，用户已确认安全基线。详见 docs/validation/001-feasibility.md。

- [x] **001-T03：验证 TLS 与快照发布可行性**
  - 依赖：001-T02。
  - 目标路径：tests/feasibility.rs、tests/feasibility/、Cargo.toml、Cargo.lock、docs/validation/001-feasibility.md。
  - 行动：在获授权实施时核对并锁定 Pingora、OpenSSL、Rust 和 KDL 依赖，验证动态证书选择、HTTP/2 ALPN、流式代理及优雅关闭扩展点。验证路由与 TLS 共用一个发布根，定义握手、请求、连接各自固定代次的时点及长连接遇到新配置的行为。
  - 预期交付物：可复现的最小可行性验证与兼容性记录。
  - 验收引用：001-AC03。
  - 计划验证：新握手读取新证书、旧连接不被关闭；不能实现时停止并请求修订，不替换成非原子的双指针发布。
  - 完成证据：tests/feasibility/ 的证书更新、跨发布握手、旧流保持、分块代理/排空验证通过；7 项集成验证全部通过。详见 docs/validation/001-feasibility.md。

- [x] **001-T04：形成实施基线**
  - 依赖：001-T03。
  - 目标路径：docs/architecture.md、docs/configuration.md、docs/validation/001-feasibility.md、Cargo.toml、Cargo.lock。
  - 行动：整理模块边界、配置契约和错误码；确定 Debian/OpenSSL 构建环境及各阶段所需检查，不引入未经批准的测试依赖或工具。原文要求的必要配置/路由/TLS/路径安全测试保留，避免覆盖追逐。
  - 预期交付物：docs/architecture.md、配置规范及依赖选型记录。
  - 验收引用：001-AC04。
  - 计划验证：确认剩余细节不会改变主要接口；所有后续阶段可以引用确定的契约。
  - 完成证据：架构文档和验证记录固定环境、依赖及后续门禁；fmt、check、严格 Clippy、test 均通过，用户已确认依赖基线。详见 docs/validation/001-feasibility.md。

## 验收标准

- [x] **001-AC01：配置语义决策表和合法/非法示例已交付且对应行为通过检查**
  - 验证步骤与判定：对原文每个示例逐项评审，明确修正或兼容行为。
  - 证据要求：记录输入或配置、实际命令/操作、预期与实际结果、日志或报告路径。
  - 验证证据：docs/configuration.md 第 1–8 节逐项确定语义与原文示例迁移；配置语法验证通过，用户已确认契约。详见 docs/validation/001-feasibility.md。

- [x] **001-AC02：模块接口、信任边界和错误模型已交付且对应行为通过检查**
  - 验证步骤与判定：检查所有歧义均有确定规则；不存在默认信任客户端 Forwarded 的行为。
  - 证据要求：记录输入或配置、实际命令/操作、预期与实际结果、日志或报告路径。
  - 验证证据：docs/architecture.md 第 2–8 节明确接口、信任边界和错误分类；复审通过，用户已确认安全基线。详见 docs/validation/001-feasibility.md。

- [x] **001-AC03：可复现的最小可行性验证与兼容性记录已交付且对应行为通过检查**
  - 验证步骤与判定：新握手读取新证书、旧连接不被关闭；不能实现时停止并请求修订，不替换成非原子的双指针发布。
  - 证据要求：记录输入或配置、实际命令/操作、预期与实际结果、日志或报告路径。
  - 验证证据：tests/feasibility/ 的证书更新、跨发布握手、旧流保持、分块代理/排空验证通过；7 项集成验证全部通过。详见 docs/validation/001-feasibility.md。

- [x] **001-AC04：docs/architecture.md、配置规范及依赖选型记录已交付且对应行为通过检查**
  - 验证步骤与判定：确认剩余细节不会改变主要接口；所有后续阶段可以引用确定的契约。
  - 证据要求：记录输入或配置、实际命令/操作、预期与实际结果、日志或报告路径。
  - 验证证据：架构文档和验证记录固定环境、依赖及后续门禁；fmt、check、严格 Clippy、test 均通过，用户已确认依赖基线。详见 docs/validation/001-feasibility.md。

## 实施验证记录

交付物已写入 docs/configuration.md、docs/architecture.md、docs/validation/001-feasibility.md、tests/feasibility.rs 和 tests/feasibility/，依赖由 Cargo.toml/Cargo.lock 固定。验证记录见 [001-feasibility.md](../../../validation/001-feasibility.md)。

实际执行 cargo fmt --check、cargo check --locked、cargo clippy --locked --all-targets -- -D warnings、cargo test --locked 和重复的 cargo test --locked --test feasibility，均通过；集成验证 7 通过、0 失败。详细首次失败、修正和复审结论已记录。用户已提交访谈 a627c1ef-9010-4a54-9bb7-c1a37a86b257，明确确认配置契约、依赖/快照/管理安全基线，并批准将原始工程、原始需求、全部已批准计划与 001 交付物纳入首次提交。任务及验收项依据实测和确认结果勾选；本阶段须独立提交成功后才关闭，不进入 002。

## 风险与缓解

Pingora 的证书回调或运行时所有权可能不支持拟议方案；以可行性验证为硬关卡，失败后先修订并获批。

## 延期决策

总计划 Q01–Q04 已在配置规范、架构文档和验证记录中解决，并获得用户明确确认。无本阶段未解决的规划阻塞项；后续生产实现仍需完成各自验收。

## 完成与实施交接契约

1. 本文仅为计划，实施需要单独授权。任务完成且所需检查通过后勾选任务；验收实际通过后勾选验收项。记录实际命令或步骤、观察结果和证据位置，不按推测批量勾选。
2. 所有必需任务、验收项及交付物完成后才可关闭本阶段。失败、跳过、阻塞或未执行的检查保持未勾选并阻止完成；范围变更必须获批。
3. 提交前同步本子计划与总计划进度。检查 Git 状态、工作区和暂存区，只暂存本阶段明确路径或已审阅代码块，不包含无关编辑、预先暂存内容或其他阶段未完成工作。无法安全分离时暂停询问。
4. 每个子计划单独创建一次完成提交，成功后才能进入依赖它的阶段；不合并多个阶段，不逐任务提交。遵循仓库约定，否则使用 `feat(rill-web-server): complete subplan NNN - phase-name`，将 NNN 和 phase-name 替换为本阶段值。
5. 验证提交成功并报告哈希；失败时说明阶段仍待提交。Git 不可用、非仓库、无身份、钩子失败或变更已提交时停止询问；不得自动初始化仓库、修改身份、绕过钩子、推送、改写历史或创建空提交。
6. 规划阶段未执行实施检查；001 的实际检查、结果及用户确认见上文和验证记录，其余阶段仍未实施。
