# Agent Note: Qoder 超预算返回 `[DONE]`，网关必须在发请求前拒绝

Status: implemented

## Problem

业主现场（2026-10-20）：

> 「我配置的 1M 上下文 128K 输出……用 dsh-hermers 中又出现了这个问题」
> （错误：`upstream_stream_stalled`，上游疑似整体故障，2 个不同账号空流）

网关日志（新诊断字段）：

```text
09:51:32 uid=01a0… 上游返回空流 — probe_error=upstream closed before first frame 已收字节=14
09:51:32 uid=019f… 上游返回空流 — probe_error=upstream closed before first frame 已收字节=14
```

**14 字节 = `data: [DONE]\n\n`**。即上游对这次请求返回了「空完成」，不是连接失败。

### 实测边界

隔离实例（`qoder:国服:DeepSeek-Flash`，未触碰业主服务）：

| 输入 | max_tokens | 结果 |
|---|---|---|
| ≈216k tokens | 8 | 成功 |
| ≈900k tokens（900,035 实测） | 128,000 | **成功**（合计 1,028,035） |
| ≈1.18M tokens | 8 | HTTP 200 + 14 字节 `[DONE]` → 网关 503 |
| 小输入 | 128,000 | 成功（`max_tokens` 单独不触发） |

⇒ 上游预算是 **输入 + `max_tokens` ≤ 1,048,576**（与 WorkBuddy 的实测上限同值）。

### 官方客户端怎么配

从官方 Qoder 客户端日志（同一账号、`dfmodel`）核对：

- `context_window=1000000`（真实窗口，不是 180000）；
- 压缩分档为 `<50% / 50–75% / 75–90%`；**69% 属于 50–75%，不压缩是正常的**
  （业主截图正是 69%，我此前误把「50% 分档」当成了压缩阈值，已更正）；
- `model_config.max_input_tokens=180000` 是发给上游的保守字段，不是窗口；
- 它靠**更高的压缩阈值**（75%/90%）给输出留余量，不会把 1M 塞满。

业主的「1M + 128K」配置本身不是错，问题是当输入接近 1M 时，
`输入 + 128K` 会越过 1,048,576；上游不报错，只回 `[DONE]`。

## Decision

在 `forward.rs` 选号之后、**发请求之前**，对 Qoder 账号做预算预检：

```text
estimate_tokens(body) + max_tokens(body) > UPSTREAM_REQUEST_TOKEN_BUDGET (1 << 20)
  ⇒ 直接返回 400 context_length_exceeded
  ⇒ 文案给出「估算输入 + max_tokens + 上限 + 当前输出下可用的输入余量」
```

- 判据放在**选号之后**：裸名请求也可能选中 Qoder 账号；
- 未 acquire 在途名额，直接返回、无需 release；
- 估算复用既有的 `estimate_tokens`（CJK ≈ 2 字符/token、ASCII ≈ 3.6–4 字符/token，
  内联图片按尺寸另计），宁可高估。

## Alternatives considered

- **收到 `[DONE]` 后原账号重试/延迟重试** — 最强理由是「不预判，试了再说」。
  否掉的原因是**这是确定性超限**：同一个请求体重试多少次都是 `[DONE]`，
  只会继续污染空流窗口、把两个好账号拖进熔断。现场 23 次空流就是这么来的。

- **由网关自动截断历史/压缩** — 最强理由是「用户无感」。否掉的原因是
  **改写了用户请求**：截断哪一段、保留哪一段没有任何可靠判据，且会悄悄丢失
  上下文；正确做法是把「太大了」这个事实交给客户端，由它按自己的策略压缩。

- **在 `/v1/models` 把 `context_length` 调小（如 900K）以让客户端提前压缩** —
  最强理由是**不改请求路径**。否掉的原因是 `context_length` 是模型窗口，
  不是「输入 + 输出」预算；不同客户端的 `max_tokens` 不同，静态调小会误伤
  小输出请求。预算必须在**看到实际 `max_tokens` 的请求路径**上算。

- **只依赖上游报错** — 最强理由是零代码。否掉的原因是上游**不报错**：
  HTTP 200 + `[DONE]`，网关无法从状态码区分「正常空回答」与「超限」；
  等发现时已经打了两轮上游、写进了空流窗口。

## Consequences

- **收益**：超限请求在 63ms 内得到明确的 `context_length_exceeded`，
  不再打上游、不再产生空流、不再触发 60 秒风暴熔断。
- **收益**：客户端收到可识别的错误码后可以触发压缩；用户也能从文案直接看到
  「输入约多少、max_tokens 多少、上限多少、应控制在多少以内」。
- **代价**：估算可能高估（CJK 2 字符/token 是偏大的一侧），极少数**刚好接近**
  上限的请求会被提前拒绝一次；压缩一次无害，漏判才会永久卡死。
- **已知上限**：预算是按 WorkBuddy 实测的 `1 << 20` 套用到 Qoder；Qoder 的精确
  边界只验证了「900K+128K 成功、1.18M 失败」两点。若上游调整预算，需要同步
  `UPSTREAM_REQUEST_TOKEN_BUDGET` 与本文。

## Verification

单测（`cargo test -p ai-gateway-router --lib qoder_budget`，3 passed）：

- `qoder_budget_detects_one_m_context_plus_128k_output` —— 1M 输入 + 128K 输出判超；
- `qoder_budget_allows_normal_requests` —— 小输入 + 128K 输出不误判；
- `qoder_budget_message_names_the_budget_and_room` —— 文案含 1048576 与 920576。

全量：core 1307、router lib 1849、server 34、四个 A/B 集成测试 20 条全绿；
`cargo check --workspace --all-targets --profile test` 通过。

实机（隔离实例 57899）：

- `1.5M 字符（实测 900,035 tokens）+ max_tokens 128000` → HTTP 200，34.9s；
- `2.39M 字符（估算 1,183,029）+ max_tokens 8` → **HTTP 400**
  `context_length_exceeded`，0.06s，未打上游。

## 关联

- [空流风暴窗口按账号区域分桶](2026-10-20-empty-stream-window-key-split-by-account-region.md)
- [Qoder session_id 使用最后一条 user 文本](2026-10-20-qoder-session-id-derived-from-last-user-message.md)
