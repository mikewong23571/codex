# Multi-Account Gateway（基于 codex-mgr）里程碑与计划（v1）

本文将 `docs/multi_account_gateway.md` 的设计落到可执行的 milestone（以 happy-path 为主），并明确每个阶段的可验收产物。

## 范围与假设

- 平台：优先 Linux（与现有 `codex-mgr` 一致）
- 验证策略：优先保证 happy-path；边界/极端场景不作为 v1 阻塞项
- 全局状态：Redis（session/sticky/token cache/lock）
- gateway 行为：薄网关（不做业务级重试、不解析 SSE）

## Milestones

### M0：代码结构与文档基线（已完成）

- `codex-mgr` 目录结构拆分，便于后续加入 `serve`
- 完成架构文档与 CLI 用户故事

验收：

- `cargo check -p codex-mgr` 通过

### M1：`serve` 骨架（HTTP server + config）

目标：启动一个可运行的 HTTP server，但先不做真实转发与鉴权。

交付：

- 新增 `codex-mgr serve` 命令（v1 读取 `$STATE_ROOT/config.toml` 作为单一可信配置来源）
- `/healthz`（或类似）端点，用于确认进程存活
- 配置加载（仅文件；不做 env/CLI 覆盖，以保持 single source of truth）

验收：

- 能启动/停止 server
- 输出 listen 地址与关键配置（脱敏）

### M2：账号池配置（pool -> labels）

目标：补齐 `gateway issue --pool <pool_id>` 的闭环依赖，使 token 绑定到可解析的账号集合。

交付：

- pool 配置（v1 单一可信来源：`$STATE_ROOT/config.toml`）：
  - `pool_id -> [label...]`（label 即 `account_id`，可定位到 `auth.json`）
- CLI（最简）：
  - `codex-mgr pools set|list|del`

验收：

- `pools set` 后配置可被 `serve` 读取
- pool 成员 label 都存在且 `auth.json` 可用

### M3：Gateway session（签发 + 校验）

目标：跑通 “client 持有 gateway_token -> gateway 校验 session -> 允许/拒绝”。

交付：

- 新增 `codex-mgr gateway issue|list|revoke`（或同等命令结构）
- `gateway issue --pool <pool_id>` 校验 pool 存在且有成员（基于 `$STATE_ROOT/config.toml`）
- Redis schema：
  - `gw:session:<gateway_token>`（TTL 对齐 expires_at）
- `serve` 在 ingress 层校验 `Authorization: Bearer <gateway_token>`：
  - miss/过期/撤销 -> 401/403

验收：

- issue 后能立刻通过鉴权；revoke 后立即失效

### M4：反向代理（非流式）

目标：把 request/response 原样透传跑通（先覆盖非 SSE）。

交付：

- header_policy（hop-by-hop 清理、禁止 gateway token 泄漏、覆写 Authorization）
- proxy：转发到 upstream 并透传响应 status/headers/body（非 SSE）

验收：

- 对一个普通 HTTP endpoint 可以透传成功

### M5：Sticky binder + account selector（不含 refresh）

目标：同一 `conversation_id` 命中同一 `account_id`，完成路由闭环。

交付：

- Redis schema：
  - `gw:sticky:<pool>:<conversation_id>` -> `account_id`（SET NX EX）
- account_selector（最简一致性哈希/取模）

验收：

- 同一 `conversation_id` 的多次请求落在同一账号

### M6：AccountTokenProvider（复用 codex-mgr 的 auth/refresh）

目标：从 `auth.json` 获取/刷新上游 token，并缓存到 Redis。

交付：

- Redis schema：
  - `gw:acct_token:<account_id>` -> `AuthMaterial`（TTL=expires-safety_window）
  - `gw:lock:acct_token_refresh:<account_id>`（单飞锁）
- provider：
  - 读取账号 `auth.json`
  - 使用 `codex-core::CodexAuth` 获取 bearer token（必要时 refresh）
  - 补充 `ChatGPT-Account-ID`（若需要）

验收：

- gateway 能在没有客户端账号 token 的情况下，独立完成上游鉴权并返回成功响应

### M7：SSE 透传（字节流 + 取消）

目标：把 SSE/流式响应按字节流透传，并正确处理断连取消。

交付：

- SSE response streaming（边读边写 + flush）
- client 断开时取消 upstream request

验收：

- client 可持续接收 SSE；断连后 gateway 资源释放（连接关闭、任务退出）

### M8：观测与最小运维能力

目标：上线可用的最小观测面。

交付：

- `tracing` 日志（包含 request_id、conversation_id hash、account_id 可选）
- 指标（可选）：请求数、上游延迟、401/5xx 计数

验收：

- 出问题时能通过日志定位到 session/sticky/token cache 相关阶段

## 依赖与风险（v1 只做记录）

- upstream 可能要求除 `Authorization` 外的额外头（例如 `ChatGPT-Account-ID`）；v1 已在 `AuthMaterial` 预留
- Redis keyspace 规模增长：sticky 必须 TTL，且建议对 conversation_id 做 hash/编码
