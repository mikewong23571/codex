# Multi-Account Gateway（薄网关）设计文档

本设计将 Codex/agent 的原始链路：

`client[account] -> auth-req -> server`

演进为：

`client[gateway-token] -> req -> gateway[multi-accounts] -> auth-req -> server`

其中 gateway 只做“最薄的一层”：

- 校验入口 `Authorization: Bearer <gateway_token>`（opaque）
- 从请求头提取 `conversation_id`（或 `session_id`）并进行 sticky 绑定（`conversation_id -> account_id`）
- 获取绑定账号的上游鉴权材料（`account_id -> AuthMaterial`）
- 覆盖上游鉴权头（通常替换 `Authorization`，必要时补充/覆盖 `ChatGPT-Account-ID`）
- HTTP + SSE **字节流**原样透传（不解析/不重组 SSE）

> 说明：本文只定义 v1 数据面（data plane）行为；控制面（token 签发、账号池维护、凭证注入）只给出最小约束，不强行规定实现形态。

## 目标与范围

### 我们要做什么

- **对 client 非侵入**：client 使用“当前客户端的 API 认证方式”，即 `Authorization: Bearer <token>`；只需把 base_url 指向 gateway，并把 token 换成 `gateway_token`。
- **多账号池**：一个 `gateway_token` 关联一个 `account_pool_id`，允许 gateway 在该账号集合内选择并 sticky。
- **会话粘滞（sticky session）**：同一 `conversation_id` 在 sticky TTL 内始终路由到同一 `account_id`。
- **SSE 流式透传**：对上游 `text/event-stream` 等流式响应，边读边写、及时 flush；client 断开后取消上游请求。

### 我们不做什么

- 不在 gateway 内做业务级重试/退避/熔断/降级（错误语义由 client ↔ server 协议负责）。
- 不解析或重组 SSE 事件（只做字节流透传）。
- 不引入传统数据库作为强依赖（全局状态只依赖 Redis）。
- 不在 gateway 内实现复杂权限体系（仅 opaque token 会话校验 + 账号池选择所需最小元数据）。

## 入口协议（Client -> Gateway）

### 认证头（与现有 client 一致）

gateway 接受并只接受以下入口鉴权方式：

- `Authorization: Bearer <gateway_token>`

其中 `gateway_token` 为 opaque 字符串，gateway **不得**将该 token 透传到上游。

> 可选：为了兼容某些 client，也可以允许 `X-Api-Key`/`Gateway-Token` 等别名，但 v1 不要求；如果要加，必须定义优先级并同样保证不泄漏到上游。

### 会话标识（用于 sticky）

gateway 从请求头提取会话标识（大小写不敏感）：

1. 优先读取 `conversation_id`
2. 若不存在，读取 `session_id`
3. 两者都不存在：该请求视为 **non-sticky**（见下文）

说明：

- 当前 Codex 客户端在关键请求中会同时携带 `conversation_id` 与 `session_id`，且值相同；gateway 只需选择其一作为 sticky key。
- `conversation_id` 必须被视为不透明字符串；gateway 不做语义解析。

### non-sticky 请求处理

当请求不含 `conversation_id/session_id` 时：

- gateway **不写入** sticky key
- 仍需要选择一个 `account_id` 以完成鉴权替换：
  - 最简策略：对 `gateway_token + path` 做一致性哈希，映射到账号池某个账号
  - 或策略化选择：例如“选择当前最空闲/剩余额度最多的账号”（需要额外的使用情况数据源，v1 可不实现）

> 该行为主要用于兼容不携带 `conversation_id` 的辅助请求（例如 usage/status 等）。

## 上游协议（Gateway -> Server）

### 头重写规则（最小集合）

在转发前，gateway 必须：

- 删除入口鉴权头：
  - `Authorization`（入口 bearer 为 `gateway_token`，必须清除）
- 删除任何可能承载 `gateway_token` 的别名头（如果未来引入）
- 清理 hop-by-hop headers（见 RFC 7230 / 9110 的连接级头；例如 `Connection`、`Keep-Alive`、`Proxy-Authenticate`、`Proxy-Authorization`、`TE`、`Trailer`、`Transfer-Encoding`、`Upgrade`）
- 覆盖/注入上游鉴权材料：
  - `Authorization: Bearer <account_access_token>`
  - 可选：`ChatGPT-Account-ID: <upstream_account_id>`（当上游需要账号选择/隔离时）

除此之外：

- 其它端到端头（如 `Accept`、`Content-Type`、`User-Agent`、`conversation_id`、`session_id` 等）按“黑名单清理 + 原样透传”为默认策略。
- `Host` 由 HTTP client/代理层按上游 URL 生成（不透传 client 的 `Host`）。

### 响应与 SSE 透传

- 对非流式响应：状态码与响应体原样透传。
- 对 SSE/流式响应：
  - gateway 以字节流方式从上游读取并写回 client
  - **不得缓冲到内存**等待完整响应
  - 必须及时 flush，避免堆积导致延迟
  - client 断开：取消上游请求（释放连接与任务）

## Redis 全局状态（Keyspace）

### 1) Gateway Session（opaque token 会话）

`gw:session:<gateway_token>` -> `GatewaySession`（TTL 对齐会话过期）

`GatewaySession`（建议 JSON）只包含 sticky 和账号选择所需的最小元数据：

- `account_pool_id: String`（此 token 允许使用的账号集合）
- `policy_key: Option<String>`（选择策略分组键；可用于一致性哈希盐/分组）
- `status: Active|Revoked`（可选；也可用“是否存在 key”表达）
- `expires_at: Timestamp`（主要用于观测；TTL 为准）

### 2) Sticky Binding（conversation -> account）

`gw:sticky:<account_pool_id>:<conversation_id>` -> `<account_id>`（TTL=sticky TTL）

- 写入必须原子：`SET key val NX EX <sticky_ttl>`
- `conversation_id` 建议做编码（例如 `base64url(sha256(conversation_id))`）：
  - 防止 key 过长
  - 防止包含空白/控制字符等导致运维工具处理异常

### 3) Account Token Cache（account -> 上游鉴权材料）

`gw:acct_token:<account_id>` -> `AuthMaterial`（TTL=token 过期 - safety_window）

`AuthMaterial`（v1 建议字段）：

- `authorization: String`（完整头值，例如 `"Bearer eyJ..."`）
- `chatgpt_account_id: Option<String>`（用于上游 `ChatGPT-Account-ID`）
- `expires_at: Timestamp`

> 备注：虽然很多上游只需要 `Authorization`，但 Codex 现有实现会在部分场景附带 `ChatGPT-Account-ID`；把它纳入鉴权材料能避免未来“薄网关”被上游契约打穿。

### 4) 单飞锁（避免并发刷新风暴）

`gw:lock:acct_token_refresh:<account_id>` -> `<request_id>`（TTL=短；PX 5~15s）

- 获取锁：`SET key val NX PX <ms>`
- 释放锁：要么完全依赖 TTL；要么使用 Lua “值匹配再 DEL”避免误删

## Happy Path（端到端闭环）

1. Client 发起请求到 gateway：
   - `Authorization: Bearer <gateway_token>`
   - `conversation_id: c-123`（可选同时带 `session_id: c-123`）
2. Gateway 校验会话：
   - `GET gw:session:<gateway_token>` -> `GatewaySession`
   - miss/过期 -> 401/403（见错误语义）
3. Sticky 绑定：
   - `GET gw:sticky:<pool>:<c-123>` -> `<account_id>`
   - miss -> `AccountSelector` 选择账号 -> `SET ... NX EX ...`
4. 获取上游鉴权材料：
   - `GET gw:acct_token:<account_id>` -> `AuthMaterial`
   - miss/临期 -> `AccountTokenProvider` 刷新 -> 写回 `gw:acct_token:*`
5. 重写并转发：
   - 清除入口 `Authorization`（gateway token）
   - 注入 `Authorization`（account token）与可选 `ChatGPT-Account-ID`
   - 清理 hop-by-hop headers
   - 转发到 upstream
6. 透传响应（含 SSE）

## 组件划分（系统层）

1. **Client（现有 Codex/agent）**
   - 不改协议：继续使用 `Authorization: Bearer <token>`、SSE 等
   - 只需：
     - base_url 指向 gateway
     - token 由原账号 token 替换为 `gateway_token`

2. **Gateway（Rust）**
   - data plane：请求校验、sticky、token 获取、头重写、反向代理（含 SSE）

3. **Redis**
   - 唯一全局状态：session、sticky、token cache、锁

4. **Upstream Server（Codex/ChatGPT backend/OpenAI API 等）**
   - 接收 gateway 转发并按账号 token 鉴权

### 组件依赖关系（运行时）

- Client -> Gateway
- Gateway -> Redis
- Gateway -> Upstream Server

## Gateway 内部模块划分（代码层）

### 1) `ingress`

职责：HTTP 入口、提取 `Authorization` / `conversation_id` / `session_id`、构造上下文。

输入：HTTP request  
输出：`IncomingRequestContext`

### 2) `header_policy`

职责：

- 清除 hop-by-hop headers
- 删除入口鉴权（gateway token）与任何 gateway 私有头
- 生成可转发 headers（黑名单策略，保留其它端到端头）

输入：headers_raw  
输出：forward_headers

### 3) `session_store`

职责：`gateway_token -> GatewaySession`

- Redis 查询 + TTL
- 可选本地 LRU（短 TTL）降低 Redis 压力

### 4) `sticky_binder`

职责：`(account_pool_id, conversation_id) -> account_id`

- `GET gw:sticky:*`
- miss -> `account_selector` -> `SET NX EX`

### 5) `account_selector`

职责：在账号池中选择初次绑定的 `account_id`

- 最简：一致性哈希/取模（只依赖 `(pool_id, policy_key, conversation_id)`）
- 可扩展：引入账号健康度/限额窗口数据（不属于 v1 data plane 必需能力）

### 6) `account_token_provider`

职责：`account_id -> AuthMaterial`

- Redis cache
- miss/临期：刷新并写回
- 使用 `gw:lock:acct_token_refresh:*` 做单飞锁

> 控制面注入的“账号刷新凭证/密钥材料”不在本文强规定；provider 只要求能产出 `AuthMaterial`。

### 7) `request_rewriter`

职责：构造上游请求

- 覆盖 `Authorization`（account token）
- 覆盖/补充 `ChatGPT-Account-ID`（若 `AuthMaterial` 提供）
- 应用 `header_policy`

### 8) `proxy`

职责：与 upstream 通信、透传 request/response（含 SSE 字节流）、取消与资源释放。

### 9) `observability`

职责：日志/指标/追踪（不影响 data plane 语义）

- `request_id`
- `conversation_id`（可脱敏/哈希）
- `account_id`（可选）
- upstream 延迟、断流原因

### 模块依赖关系（建议单向）

- `ingress`
  - depends on: `header_policy`, `session_store`, `sticky_binder`, `account_token_provider`, `request_rewriter`, `proxy`, `observability`
- `session_store`
  - depends on: `redis_client`, (optional) `lru_cache`
- `sticky_binder`
  - depends on: `redis_client`, `account_selector`
- `account_token_provider`
  - depends on: `redis_client`, `auth_client`(刷新逻辑), (optional) `singleflight/lock`
- `proxy`
  - depends on: `http_client`(hyper/reqwest), `observability`
- `request_rewriter`
  - depends on: `header_policy`

约束：

- `header_policy` 纯函数化（不依赖 Redis/业务模块）
- `account_selector` 纯策略（不依赖 Redis/IO）

## 关键数据结构（概念）

### `IncomingRequestContext`（内存）

- `request_id: String`
- `gateway_token: String`
- `conversation_id: Option<String>`（来自 `conversation_id/session_id`）
- `method, path, query`
- `headers_raw`
- `body_stream`（可选）

### `GatewaySession`（Redis）

见上文 Keyspace。

### `AuthMaterial`（Redis）

见上文 Keyspace。

## 模块接口（概念）

```text
SessionStore
  get_session(gateway_token) -> Option<GatewaySession>

StickyBinder
  bind(session, conversation_id) -> account_id
  select_non_sticky(session, request_fingerprint) -> account_id

AccountTokenProvider
  get_auth_material(account_id) -> AuthMaterial

Proxy
  forward(upstream_request) -> upstream_response_stream
```

## 错误语义（最小集合）

gateway 不做业务级异常处理，但必须返回明确 HTTP：

- `401 Unauthorized` / `403 Forbidden`：`gw:session:<token>` 不存在/过期/被撤销
- `503 Service Unavailable`：Redis 不可用（或必须的 Redis 操作失败）
- `502 Bad Gateway`：上游连接失败或协议错误
- `504 Gateway Timeout`：上游超时（如果配置了上游超时）

> 若上游返回 `401/403`：gateway 透传响应；可选做缓存失效（evict `gw:acct_token:<account_id>`）但不自动重放请求。

## 配置项（最小集合）

- Redis 连接信息
- upstream base URL
- sticky TTL（例如 2h/24h）
- token safety window（例如 120s）
- 上游超时/连接池（SSE 场景需谨慎设置）
- 账号池配置（pool -> labels 映射）

## 配置与优先级（v1：单一可信来源）

为避免 “CLI vs env vs 文件” 多层覆盖导致不可预期行为，v1 约定 **只允许一处可配置**（single source of truth）：

- 配置文件：`$STATE_ROOT/config.toml`（默认 `~/.codex-mgr/config.toml`）
- 不支持 env 覆盖、不支持 `serve` 的 CLI flags 覆盖（`gateway issue` 的 `--pool/--ttl` 属于一次性输入，不属于持久配置）

唯一的定位方式：

- 使用 `codex-mgr --state-root <dir> ...` 改变 `$STATE_ROOT`（等价于改变配置文件路径）

### 配置文件结构（建议）

```toml
[gateway]
listen = "127.0.0.1:8787"
upstream_base_url = "https://chatgpt.com/backend-api/"
redis_url = "redis://127.0.0.1:6379"
sticky_ttl_seconds = 7200
token_safety_window_seconds = 120

[pools.default]
labels = ["whale-pullfast", "other-label"]
# policy_key 可选；用于 selector 的盐/分组
# policy_key = "default"
```

## 基于 codex-mgr 的实现选型（v1）

本方案计划直接在 `codex-mgr` 内新增 `serve` 命令实现 gateway（单二进制），并复用现有多账号管理能力。

### Gateway token 签发与存储

v1 选择 **Redis 作为 gateway session 的唯一权威存储**：

- `codex-mgr` 负责签发 `gateway_token`，并写入 `gw:session:<gateway_token>`（TTL 对齐 expires_at）
- gateway 校验请求时只读 Redis（不依赖本地 session 文件），避免双写一致性与多进程并发问题

> 本地存储仅用于 `codex-mgr` 的自身配置/元数据（例如账号池配置文件），不承担 session 权威来源。

### 账号池与账号标识（闭环定义）

为保证 “token -> pool -> account -> auth.json -> 上游鉴权” 跑通，v1 约定：

- `account_id`（Redis key 中使用的账号标识）= `codex-mgr` 的本地 `label`
  - 对应目录：`$ACCOUNTS_ROOT/<label>/`
  - 对应认证文件：`$ACCOUNTS_ROOT/<label>/auth.json`
- `AuthMaterial.chatgpt_account_id`（用于上游 `ChatGPT-Account-ID`）来自该 `auth.json` 中的账号信息（若存在）
- `account_pool_id` 表示一个账号集合（pool），其成员是若干 `label`（即 `account_id`）

这样 gateway 在命中 `account_id` 后无需额外映射即可定位 refresh 凭证并获取 access token。

### 复用 codex-mgr 的能力边界

- **账号凭证来源**：继续使用每个账号的 `auth.json`（由 `codex-mgr login --label ...` 产出）
- **refresh token 流程**：复用 `codex-core` 的 `CodexAuth`（`get_token()` / `refresh_token()`），由 gateway 的 `AccountTokenProvider` 驱动
- **usage/rate limit**：复用 `codex-backend-client::Client::get_rate_limits()`（需要时用于初次选择或健康检查；sticky 命中时不参与路由）

### 账号池配置（v1）

为保持实现简单、避免额外强依赖，v1 选择：

- pool 配置在 `codex-mgr` 的 **`$STATE_ROOT/config.toml`**（单一可信来源）
- `gateway_token` session 存 Redis（权威），session 只引用 `account_pool_id`

> 若未来需要多机多实例且希望统一配置，可将 pool 映射迁移到 Redis（例如 `gw:pool:<pool_id>`），但不作为 v1 必需项。

### 技术栈（建议）

- Runtime：Rust + `tokio`
- HTTP ingress：`axum`（路由/中间件） + `hyper`（底层 IO）；或纯 `hyper`（更薄，但开发成本更高）
- Upstream client：`hyper` + `hyper-rustls`（便于做流式 body 透传与取消）
- Redis：`redis`(redis-rs) async（覆盖 `GET`、`SET NX EX/PX`、Lua 脚本）
- Observability：`tracing` + `tracing-subscriber`；可选 `tower-http`(Trace/Timeout)

## 安全与隐私要点（v1 必须满足）

- gateway token **绝不**转发到上游；日志中也不得输出明文 token。
- 清理 hop-by-hop headers，避免连接级语义被滥用。
- Redis key 必须 TTL，避免 keyspace 无界增长（尤其 sticky）。
- `AuthMaterial` 属于高敏感数据：
  - 建议 Redis 仅内网可达并启用访问控制
  - 建议 value 加密/或只缓存短期 access token（不存长效 refresh 凭证）

## 验收标准（Definition of Done）

- client 使用 `Authorization: Bearer <gateway_token>` 访问 gateway，SSE 流可正常返回
- 同一 `conversation_id` 的多次请求命中同一 `account_id`（sticky 生效）
- gateway 不向上游泄漏 `gateway_token`（抓包/日志验证）
- Redis key 都有 TTL（验证不会无限增长）
