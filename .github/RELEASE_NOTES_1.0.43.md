# AI Gateway 1.0.43

**主题：修掉「对话一直在中断」——上游用正常帧的形状宣告本轮失败，网关却把它当成答完了。**

---

## 一、业主原话

> 为什么这个模型在对话过程中一直在中断?你看看咋回事

界面上的表现是 DSH 打出「本轮运行失败」+ `Provider finish_reason: error`（`P1_AI_ERROR`），
然后自动续写、再失败，看起来就是「一直在中断」。

---

## 二、根因：上游的超时收尾被读成了正常收尾

workbuddy 后端对单轮生成有约 **900 秒**的硬上限。超时后它**不是**断连、也**不是**回 HTTP 错误，
而是用**完全正常的 SSE 帧形状**宣告这一轮失败：

```text
HTTP 200
data: {"choices":[{"delta":{"content":"……写了一半……"},"index":0}]}
data: {"choices":[{"delta":{},"finish_reason":"error","index":0}]}   ← 失败语义
data: [DONE]
```

实测到的三次截断：**900.572s / 900.701s / 892.7s**，形状完全一致。

### 旧逻辑错在哪

网关把**任何非空**的 `finish_reason` 都当作「本轮正常结束」。于是：

| 层 | 旧行为 | 后果 |
|---|---|---|
| 网关流式 | 原样透传 `finish_reason:"error"` | 客户端收到「答完了，结束原因是 error」 |
| pi-ai | `mapStopReason` 落到默认分支 | `Provider finish_reason: error` |
| dsh-llm-pi-ai | `classifyPiAiError` 认不出 | 归类 `PI_AI_ERROR` |
| dsh-llm-retry | `PI_AI_ERROR` **不在**可重试码里 | **直接放弃，不换账号、不重试** |
| dsh-agent-loop | 抛 `LlmError` | UI「本轮运行失败」 |

业主现场网关日志正是这么记的（**两次真实故障都被记成「正常结束」**）：

```text
2026-10-12 21:29:11.969 +08:00 … chat uid=1f3c55e5-… model=step-5-preview:
  流结束（已收 12734 个有效帧 / 3187410 字节，saw_done=true，saw_finish_reason=true）
2026-10-12 21:55:57.657 +08:00 … chat uid=e2891116-… model=step-5-preview:
  流结束（已收 10770 个有效帧 / 2782203 字节，saw_done=true，saw_finish_reason=true）
```

⇒ 网关侧**没有任何失败记录**，排查时只剩客户端那句 `Provider finish_reason: error`。

---

## 三、修法

把 `finish_reason:"error"` 认定为**失败语义**（只认这一个取值；`stop` / `length` /
`tool_calls` / `content_filter` / 空串都仍是正常收尾），并在四条链路上统一：

1. **chat 流式** —— 不再透传失败帧，改为向客户端补一帧**可重试**的 error：

   ```text
   data: {"error":{"code":"upstream_stream_truncated",
                  "message":"upstream stream ended before a completion event; this turn may be incomplete",
                  "type":"upstream_error"}}
   data: [DONE]
   ```

   文案里刻意保留 `stream ended before` —— dsh-llm-pi-ai 的 `classifyPiAiError` 用
   `/stream ended (?:before|without)\b/i` 归类，命中即 **TRANSPORT**（可重试）⇒ 会走
   **换账号重试**，而不是把整轮丢给用户。

2. **chat 非流式** —— 返回 **502**（`上游以 finish_reason=error 结束本轮（未给出回答）`），
   走既有的上游故障分类与账号轮换。

3. **`/v1/messages`、`/v1/responses` 非流式** —— 同上 502（此前分别是 `end_turn` /
   `status:"completed"`）。

4. **`/v1/messages`、`/v1/responses` 流式** —— 失败语义记进状态机的失败出口，
   客户端收到 `event: error` / `response.failed`，**而不是** `message_stop` /
   `response.completed`。

   > 这一条修的是**自相矛盾**：同一个上游故障，**不带** `[DONE]` 时网关已经会发 error 事件，
   > **带** `[DONE]` 时却宣告「正常答完」——而 `[DONE]` 只是终止字节，不该改变语义。

### 顺带补上的可观测性

两条姊妹端点的失败收尾分支**此前完全没有日志**（这正是本次排查最缺的凭据）。现在都会记：

```text
messages uid=… product=workbuddy model=…: 流以失败收尾，已向客户端发 error 事件
  （已收 2 个有效帧，saw_done=true，saw_finish_reason=false）：
  upstream error: 上游以 finish_reason=error 结束本轮（未给出回答）
```

---

## 四、实测记录（独立端口 + 独立数据目录 + 假上游）

新（:57910）/ 旧（:57920）双实例对打，上游按真实形状回帧：

| # | 形状 | 旧 | 新 |
|---|---|---|---|
| A | `/v1/messages` 流式，**无** `[DONE]` | `message_delta{stop_reason:"end_turn"}` + `message_stop` | `event: error` ✅ |
| B | `/v1/messages` 流式，**有** `[DONE]` | 同上（宣告答完） | `event: error` ✅ |
| C | `/v1/responses` 流式，**有** `[DONE]` | `response.completed`（`status:"completed"`） | `response.failed` ✅ |
| D | `/v1/responses` 流式，**无** `[DONE]` | `response.completed` | `response.failed` ✅ |
| E | 非流式 `/v1/chat/completions` | 200 + `finish_reason:"error"` | **502** ✅ |
| F | 非流式 `/v1/messages` | 200 + `end_turn` | **502** ✅ |
| G | 非流式 `/v1/responses` | 200 + `"status":"completed"` | **502** ✅ |

零回归（逐字节比对，新 == 旧）：

- 正常 `stop` 流：`/v1/chat/completions`、`/v1/messages`、`/v1/responses` 全部一致；
- 真错误帧（`{"error":{"code":"3012",…}}`）：三个端点全部一致（本来就已正确）；
- 非流式正常 `stop`：一致。

账号记账：每次失败 `err_total` +1 并喂熔断计数，`breaker_threshold` 默认 3 ⇒
**单次截断不会禁用账号**（900s 截断是上游全局的，不会把号池打成全灭）。

---

## 五、升级说明

直接覆盖安装即可，配置与账号不受影响。装完**重启网关**让新网关生效。

> 注意：这是**让失败变得可重试**，不是让 900s 上限消失。真正会中断的轮次仍会中断，
> 但现在客户端会**自动换账号重试**，而不是把「本轮运行失败」直接甩给用户。

## 校验

解压后可用仓库内 `keys/` 的 minisign 公钥验证 `.sig`。
