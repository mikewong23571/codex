# codex-mgr（含 gateway/serve）CLI 用户故事

本文梳理基于 `codex-mgr` 的命令行用户故事（以 happy-path 为主），用于驱动后续 `serve`（薄网关）能力的 CLI 设计与实现分解。

## 角色

- **Operator（本机管理员）**：维护账号池、启动 gateway、签发 gateway token。
- **Client（AI coding agent / Codex CLI / 其他 SSE 客户端）**：只持有 `gateway_token`，以 `Authorization: Bearer <gateway_token>` 方式访问 gateway。

## 账号管理（现有能力）

### US-1：新增一个 ChatGPT 账号（带本地标识）

作为 Operator：

- 我希望执行 `codex-mgr login --label <label>` 来新增一个账号，并将该账号的 `auth.json` 存储到 `~/.codex-accounts/<label>/auth.json`。
- 成功条件：
  - label 校验通过且唯一
  - upstream `codex login` 完成且 `auth.json` 中存在 `refresh_token`

### US-2：查看所有账号与缓存的 usage 快照

作为 Operator：

- 我希望执行 `codex-mgr accounts list`，看到对齐的表格输出（label/email/weekly/5h/age/status）。
- 我希望执行 `codex-mgr accounts list --json`，得到机器可读输出。

### US-3：删除一个账号的登录信息

作为 Operator：

- 我希望执行 `codex-mgr accounts del <label>` 删除该账号的 `auth.json`（用于订阅到期/账号失效的场景），但共享配置与历史不删除。

## 启动 Codex（现有能力）

### US-4：自动选择“余量最大”的账号启动 Codex（会话内不切换）

作为 Operator：

- 我希望执行 `codex-mgr run --auto -- <codex args...>`，由 `codex-mgr` 选择一个账号并启动 upstream `codex`。
- 选择规则（已约定）：
  1) 优先 weekly window 剩余百分比最大
  2) 再按 5h window 剩余百分比最大
  3) 再按 label 字典序作为 tie-break
- 会话约束：
  - 同一个 upstream `codex` 进程生命周期内不切换账号

### US-5：指定账号启动 Codex

作为 Operator：

- 我希望执行 `codex-mgr run --label <label> -- <codex args...>` 来强制使用某个账号。

## Gateway（新增能力：serve + token）

### US-6a：定义/更新账号池（pool）

作为 Operator：

- 我希望能够把若干已登录账号（label）组织成一个 `pool_id`，供 gateway token 绑定与路由使用。
- 最小命令集合（提议）：
  - `codex-mgr pools set <pool_id> --labels <label1,label2,...>`
  - `codex-mgr pools list [--json]`
  - `codex-mgr pools del <pool_id>`
- 配置落盘（v1 约定）：写入 `$STATE_ROOT/config.toml`（默认 `~/.codex-mgr/config.toml`）
- 成功条件：
  - pool 中的 label 都存在（对应 `~/.codex-accounts/<label>/auth.json` 可读）
  - `serve` 能根据 `pool_id` 找到成员列表

### US-6：启动 gateway（SSE 透传）

作为 Operator：

- 我希望执行 `codex-mgr serve` 启动一个 HTTP gateway（v1 读取 `$STATE_ROOT/config.toml` 作为单一可信配置来源）：
  - 接受 `Authorization: Bearer <gateway_token>`
  - 读取 `conversation_id`（或 `session_id`）并 sticky 到某个账号
  - 将请求转发到 upstream（含 SSE 字节流透传）
- 成功条件：
  - gateway 启动后输出 listen 地址、上游地址、Redis 连接信息（脱敏）
  - client 可以通过 gateway 正常完成一次 SSE 会话（DoD 见 `docs/multi_account_gateway.md`）

### US-7：签发一个 gateway token（绑定账号池）

作为 Operator：

- 我希望执行 `codex-mgr gateway issue --pool <pool_id> --ttl <duration> [--note <text>]`：
  - 生成 `gateway_token`（opaque）
  - 写入 Redis：`gw:session:<gateway_token> -> GatewaySession`（TTL 对齐）
  - 输出 token（可选同时输出一个 “token_id” 便于撤销）
  - 校验 `pool_id` 存在且有成员（见 US-6a）

### US-8：查看已签发的 gateway token

作为 Operator：

- 我希望执行 `codex-mgr gateway list` 查看当前可用的 gateway session（最好支持 `--json`）。

> v1 可接受实现为 Redis `SCAN gw:session:*`（适合小规模）；若后续规模变大，再引入 session index（例如 set/list）。

### US-9：撤销一个 gateway token

作为 Operator：

- 我希望执行 `codex-mgr gateway revoke <token-or-id>`，使该 token 立即失效（删除对应 `gw:session:*`）。

## Client 使用方式（非侵入）

### US-10：Client 在不改代码的前提下接入 gateway

作为 Client：

- 我希望只通过配置将 base_url 指向 gateway，并将 bearer token 替换为 `gateway_token`，其余请求与 SSE 处理保持不变。
- 约束：
  - gateway 不得要求 client 使用额外的私有 header（v1 入口仅 `Authorization: Bearer <gateway_token>`）
