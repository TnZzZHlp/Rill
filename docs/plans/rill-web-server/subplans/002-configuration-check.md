# 002：完整配置系统与 check

## 元数据

- 总计划：[Rill Web 服务器实施计划](../README.md)
- 阶段 ID：002
- 实施状态：已验收；复审、测试通过，阶段关闭以本次独立完成提交成功为准
- 依赖：001
- 目标引用：G01
- 决策引用：总计划 D01–D09，配置契约以 001 确认版本为准；用户在 002 明确要求使用 Clap 处理 CLI。

## 目标与范围

以同一套配置构建管线支持启动、check 和未来 Reload，输出可定位错误。

包含本阶段任务明确列出的能力。排除其他阶段的实现及原文非目标功能；不得借本阶段扩大范围。

## 前置条件

阶段 001 已通过验收并完成各自独立提交。
实施前阅读目标文件与工作区状态，保留现有用户编辑。验证需要真实配置、证书或上游时，在受控环境准备，不使用真实私钥作为仓库内容；新增依赖或测试工具须遵守批准边界。

## 设计与模块边界

- 实际交付路径：src/config/、src/cli/、src/lib.rs、src/main.rs、Cargo.toml、Cargo.lock、examples/、tests/configuration.rs，以及配置/架构规范和本阶段验证记录。
- loader 保存来源文件及跨度，parser 产生配置模型，profile 负责确定性展开，validate 检查全局约束；资源准备由专门模块提供。check 不连接管理 Socket、不发布配置、不绑定服务端口。
- main/CLI 仅编排，不持有业务规则；模块通过明确接口通信，不直接改写其他模块内部状态。
- 外部错误必须可恢复、可定位并脱敏。所有配置更新复用统一验证与准备流程，禁止绕过运行时事务边界。

## 实施任务

- [x] **002-T01：搭建薄入口与配置模型**
  - 依赖：001。
  - 目标路径：src/config/、src/cli/check.rs、src/cli/mod.rs、src/main.rs、Cargo.toml、Cargo.lock；仅修改本任务相关模块。
  - 行动：建立批准的模块结构和依赖锁定，main 仅编排 CLI 与启动；定义配置、来源跨度和分层错误，严格拒绝未知节点、参数、重复或非法类型。
  - 预期交付物：可编译工程及 KDL 解析器。
  - 验收引用：002-AC01。
  - 计划验证：执行 cargo fmt --check、cargo check；检查错误包含文件、配置项和位置。
  - 完成证据：src/config/parser/、model.rs、source.rs、error.rs 实现严格 KDL 类型/结构校验和来源诊断；fmt、check、严格 Clippy 及相关集成验证通过。详情见 docs/validation/002-configuration-check.md。

- [x] **002-T02：实现导入与 Profile 规范化**
  - 依赖：002-T01。
  - 目标路径：src/config/、src/cli/check.rs、src/cli/mod.rs、src/main.rs、Cargo.toml、Cargo.lock；仅修改本任务相关模块。
  - 行动：实现文件相对 import、确定性 glob 排序、嵌套导入、循环和重复定义检测；按 001 实现 use、继承覆盖及简写展开。
  - 预期交付物：单一标准化配置表示。
  - 验收引用：002-AC02。
  - 计划验证：检查多层导入、重复定义、未知 Profile、循环和局部覆盖；覆盖 T12 的配置部分、T13。
  - 完成证据：loader.rs、glob.rs、profile.rs 实现嵌套确定性导入、文件身份去重/循环检查、前向 Profile 引用与继承；T12 配置重建和 T13 验证通过。详情见 docs/validation/002-configuration-check.md。

- [x] **002-T03：实现语义校验与资源准备接口**
  - 依赖：002-T02。
  - 目标路径：src/config/、src/cli/check.rs、src/cli/mod.rs、src/main.rs、Cargo.toml、Cargo.lock；仅修改本任务相关模块。
  - 行动：校验域名/路由冲突、动作互斥、上游 URL、超时、TLS 引用和静态根目录配置；定义 PreparedSnapshot 构建接口，后续 TLS 模块接入真实证书校验，未接入前不能宣称完整 check 已交付。
  - 预期交付物：可复用验证管线及资源准备边界。
  - 验收引用：002-AC03。
  - 计划验证：检查 T14 与所有引用错误；禁止 check 跳过未知或未实现配置而返回成功。
  - 完成证据：validate/ 和 prepare.rs 实现域名/路由冲突、上游 URL/字面量策略、TLS 引用及统一资源准备边界；T14 和不支持资源不能误报成功的验证通过。详情见 docs/validation/002-configuration-check.md。

- [x] **002-T04：完成 CLI check 及必要回归**
  - 依赖：002-T03。
  - 目标路径：src/config/、src/cli/check.rs、src/cli/mod.rs、src/main.rs、Cargo.toml、Cargo.lock；仅修改本任务相关模块。
  - 行动：实现 --config 与退出码，提供单文件/多文件示例；用必要的配置解析和继承测试验证失败位置。阶段性 check 明确当前资源校验范围，最终完整资源检查由 004/005 补齐。
  - 预期交付物：可运行 check、基础配置样例。
  - 验收引用：002-AC04。
  - 计划验证：合法基础配置成功，非法导入和语义返回非零；验证 check 不影响活动实例。
  - 完成证据：Clap 驱动 check 已实现；两个 examples 入口成功；真实 CLI 的退出码、位置和无监听/无控制连接副作用均通过；15 项配置验证与 7 项既有验证共 22 项通过。详情见 docs/validation/002-configuration-check.md。

## 验收标准

- [x] **002-AC01：可编译工程及 KDL 解析器已交付且对应行为通过检查**
  - 验证步骤与判定：执行 cargo fmt --check、cargo check；检查错误包含文件、配置项和位置。
  - 证据要求：记录输入或配置、实际命令/操作、预期与实际结果、日志或报告路径。
  - 验证证据：src/config/parser/、model.rs、source.rs、error.rs 实现严格 KDL 类型/结构校验和来源诊断；fmt、check、严格 Clippy 及相关集成验证通过。详情见 docs/validation/002-configuration-check.md。

- [x] **002-AC02：单一标准化配置表示已交付且对应行为通过检查**
  - 验证步骤与判定：检查多层导入、重复定义、未知 Profile、循环和局部覆盖；覆盖 T12 的配置部分、T13。
  - 证据要求：记录输入或配置、实际命令/操作、预期与实际结果、日志或报告路径。
  - 验证证据：loader.rs、glob.rs、profile.rs 实现嵌套确定性导入、文件身份去重/循环检查、前向 Profile 引用与继承；T12 配置重建和 T13 验证通过。详情见 docs/validation/002-configuration-check.md。

- [x] **002-AC03：可复用验证管线及资源准备边界已交付且对应行为通过检查**
  - 验证步骤与判定：检查 T14 与所有引用错误；禁止 check 跳过未知或未实现配置而返回成功。
  - 证据要求：记录输入或配置、实际命令/操作、预期与实际结果、日志或报告路径。
  - 验证证据：validate/ 和 prepare.rs 实现域名/路由冲突、上游 URL/字面量策略、TLS 引用及统一资源准备边界；T14 和不支持资源不能误报成功的验证通过。详情见 docs/validation/002-configuration-check.md。

- [x] **002-AC04：可运行 check、基础配置样例已交付且对应行为通过检查**
  - 验证步骤与判定：合法基础配置成功，非法导入和语义返回非零；验证 check 不影响活动实例。
  - 证据要求：记录输入或配置、实际命令/操作、预期与实际结果、日志或报告路径。
  - 验证证据：Clap 驱动 check 已实现；两个 examples 入口成功；真实 CLI 的退出码、位置和无监听/无控制连接副作用均通过；15 项配置验证与 7 项既有验证共 22 项通过。详情见 docs/validation/002-configuration-check.md。

## 实施验证记录

用户授权实施 002、完成后复审、测试并在通过后提交；实施期间另明确要求使用 Clap。已执行 cargo fmt --check、cargo check --locked、cargo clippy --locked --all-targets -- -D warnings、cargo test --locked，全部通过，共 22 项集成验证（002 配置 15 项、001 既有 7 项）。详细输入、观察结果、复审修正和证据映射见 [002-configuration-check.md](../../../validation/002-configuration-check.md)。

阶段性 check 对 TLS/静态 root 返回 CONFIG_UNSUPPORTED，不冒充完成资源校验；生产 run/reload 尚未提供。已修复深层 KDL 栈溢出并保留必要回归；入口符号链接、纯 HTTP/TLS 站点共存和地址策略边界也已复核。任务和验收项据真实结果勾选，本阶段独立提交成功后关闭，不进入 003。

## 风险与缓解

语法解析成功不等于配置可用；将真实资源准备复用于 check/start/reload，避免三套校验漂移。

## 延期决策

无本阶段未解决阻塞项。真实证书/根目录资源准备由 004/005 接入，DNS、连接和运行时代理行为按后续阶段验收；这些范围不被本阶段 check 的成功结果覆盖。

## 完成与实施交接契约

1. 本文仅为计划，实施需要单独授权。任务完成且所需检查通过后勾选任务；验收实际通过后勾选验收项。记录实际命令或步骤、观察结果和证据位置，不按推测批量勾选。
2. 所有必需任务、验收项及交付物完成后才可关闭本阶段。失败、跳过、阻塞或未执行的检查保持未勾选并阻止完成；范围变更必须获批。
3. 提交前同步本子计划与总计划进度。检查 Git 状态、工作区和暂存区，只暂存本阶段明确路径或已审阅代码块，不包含无关编辑、预先暂存内容或其他阶段未完成工作。无法安全分离时暂停询问。
4. 每个子计划单独创建一次完成提交，成功后才能进入依赖它的阶段；不合并多个阶段，不逐任务提交。遵循仓库约定，否则使用 `feat(rill-web-server): complete subplan NNN - phase-name`，将 NNN 和 phase-name 替换为本阶段值。
5. 验证提交成功并报告哈希；失败时说明阶段仍待提交。Git 不可用、非仓库、无身份、钩子失败或变更已提交时停止询问；不得自动初始化仓库、修改身份、绕过钩子、推送、改写历史或创建空提交。
6. 规划阶段未执行实施检查；002 已单独授权并实际验证，结果见本阶段验证记录，其余未开始阶段保持未实施。
