# 1.0.53

## Qoder：请求体与官方客户端对齐（本次「还是用不了」的直接修复）

1.0.52 修掉了会话/熔断/预算三类问题，但 Qoder 在 DSH Hermes 上仍然报
`upstream_stream_stalled`。这次从官方 Qoder 客户端的请求体构造函数
（`qoder-worker-runtime` 的 `bve`）逐字段核对，发现**网关的 agent 请求体与官方
形状不一致**——上游对这种请求**不报错**，只回 `HTTP 200 + 14 字节 data: [DONE]`，
于是网关把它当成空流、换号、熔断。

已按官方形状补齐：

| 字段 | 之前 | 现在（官方形状） |
|---|---|---|
| `system` | 混在 `messages` 里 | **独立数组**，`messages` 不再带 system |
| `messages[].contents` | 无 | 文本/图片/`tool_use` 块数组 |
| `source` / `version` | 无 | `1` / `"3"` |
| `is_retry` / `task_id` | 无 | `false` / `"common"` |
| `aliyun_user_type` | `personal_professional_trial` | **空串**（官方值） |
| `parameters` | 无 | 带 `max_tokens`（取客户端传值） |
| `chat_context.text` | `{type,text}` 对象 | **字符串** |
| `chat_context.extra.modelConfig` | 完整 10 字段 | `{key, is_reasoning}`（完整那份留在顶层 `model_config`） |
| `tools` | 没有就省略 | **恒为数组** |

另外两处按「**转换**而不是丢弃」处理：

- **`type: "custom"`（grammar）工具**：转成 function 工具（字符串输入 +
  格式说明），模型仍能调用它；官方客户端从不发这种形状，上游对它只回 `[DONE]`。
- **工具结果里的图片**：保留在 `content`/`contents` 块数组里（视觉模型照常
  能看到图片）；assistant 历史里的 `reasoning_content` 也按官方形状保留。

## 上下文预算（1.0.52 已加入，继续保留）

Qoder 对「输入 + `max_tokens` > 约 1,048,576」返回 `200 + [DONE]`。网关现在
发请求前预检，超限直接返回 `400 context_length_exceeded`，并给出可用输入余量。
实测：900K 输入 + 128K 输出成功；1.18M 输入在 63ms 内被明确拒绝。

## 其它（1.0.52 内容，一并包含）

- Qoder `session_id` 用首条 user 文本 + 账号 uid 派生（整段对话稳定，换号不串会话）；
- 空流风暴按请求区域分桶（两个账号都空流时真正熔断）；
- 在途名额满时有界排队（`pool.in_flight_wait_ms`，默认 5s），并发重试不再成片 503；
- `/v1/models` 下发 Qoder 倍率并在启动时预热；
- 新增「允许局域网访问」开关（`lan_access`），保留手写 `0.0.0.0` 监听。

## 升级提示

- 安装 1.0.53 后重启应用即可；无需改配置。
- 若配置为「1M 上下文 + 128K 输出」，请把实际输入控制在约 900K 以内（或调小
  输出），否则会收到明确的 `context_length_exceeded`，而不是空流 503。