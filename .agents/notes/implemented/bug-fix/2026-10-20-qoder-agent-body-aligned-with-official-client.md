# Agent Note: Qoder agent 请求体与官方客户端对齐——`[DONE]` 的直接原因

Status: implemented

## Problem

1.0.52 已修掉 session / 熔断 / 上下文预算三类问题，但业主在 DSH Hermes 上仍报：

> 「重试延迟：1841毫秒 / 失败原因：503 upstream_stream_stalled（2 个不同账号空流）」

运行中的网关确认为 1.0.52（hash `E2A86AEE…`，含上下文预检），日志里
**没有**「请求预算超限」，但两条账号的失败都是
`probe_error=upstream closed before first frame 已收字节=14`
—— 14 字节就是 `data: [DONE]`。也就是说：**请求没超预算，上游却静默回空完成**。

### 直接证据：官方请求体构造函数

从官方 Qoder 客户端（`@qoder-ai/qoder-cn-agent-sdk` 的
`qoder-worker-runtime.obf.mjs`）里定位到 agent 请求体构造函数 `bve`，
逐字段对照出网关的差异：

| 字段 | 网关（对齐前） | 官方 `bve` |
|---|---|---|
| `system` | 混在 `messages` 里 | **独立数组**（`messages` 不含 system） |
| `messages[].contents` | 无 | 文本 / 图片 / `tool_use` 块数组 |
| `source` | 无 | `1` |
| `version` | 无 | `"3"` |
| `is_retry` | 无 | `false` |
| `task_id` | 无 | `"common"` |
| `aliyun_user_type` | `personal_professional_trial` | **空串** |
| `parameters` | 无 | `{max_tokens,...}` |
| `chat_context.text` | `{type,text}` 对象 | **字符串** |
| `chat_context.extra.modelConfig` | 完整 10 字段 | `{key, is_reasoning}` |
| `tools` | 没有就省略 | **恒为数组** |

### 为什么简单请求能过、复杂请求不行

上游对这些形状差异**不报错**：缺字段时按默认值回落。简单文本请求恰好落在
能过的组合里；而 DSH Hermes 的请求（长 system + 工具 + 历史 reasoning + 可能
的 grammar 工具/图片）触发了上游的静默 `[DONE]`。

## Decision

按官方形状构造 `agent_body`（`translate.rs`）：

1. `developer`/`system` 消息**拆到独立的 `system` 数组**，`messages` 不再含 system；
2. 每条消息带 `contents` 块（`text` / `image_url` / `tool_use`），与官方
   `uPc` / `gPc` 输出同形；
3. 顶层补 `source:1` / `version:"3"` / `is_retry:false` / `task_id:"common"`；
4. `aliyun_user_type` 改为**空串**（官方值）；
5. `parameters.max_tokens` 取客户端传的 `max_tokens` / `max_completion_tokens`；
6. `chat_context.text` / `originalContent` 改成**字符串**，
   `extra.modelConfig` 只带 `{key, is_reasoning}`（完整 10 字段仍在顶层）；
7. `tools` 恒为数组（没有就是 `[]`）。

两处**转换而不丢弃**（业主明确要求「应该是支持转换」）：

- `type:"custom"`（grammar 工具）→ 转成 function 工具：`input: string` +
  把 grammar 定义写进参数说明。模型仍能调用，只是失去语法强制；
- 工具结果里的图片 → 保留在 `content`/`contents` 块数组；assistant 的
  `reasoning_content` 按官方形状写回同名字段。

## Alternatives considered

- **继续逐个字段尝试（只补 `system` 或只补 `parameters`）** — 最强理由是
  改动最小、可快速验证。否掉的原因是**没有判据**：上游对缺字段不报错，
  单字段改动无法证明是哪一处触发的；而官方 `bve` 已经给出完整形状，
  逐字段猜等于把同一个缺陷留一半。
- **丢弃 `custom` 工具（最初实现）** — 最强理由是简单、绝不会因它触发 `[DONE]`。
  否掉的原因是**功能静默消失**：用户开了 grammar 采样，转换后模型仍能调用；
  丢掉则模型完全看不到这个工具。业主原话「不应该是过滤，应该是支持转换」。
- **把 OpenAI messages 整体转成 Anthropic 形状** — 最强理由是官方内部
  对话是 Anthropic 风格。否掉的原因是**多做了一层**：官方 `HrA`/`uPc`/`gPc`
  最终产出的仍是 OpenAI 风格（`role:"tool"` + `tool_call_id` + `tool_calls`），
  只是多带 `contents` 块。照官方最终形状即可，不必引入中间表示。

## Consequences

- **收益**：DSH Hermes 的复杂请求（system + 工具 + reasoning 历史 + 图片）
  不再被上游静默回 `[DONE]`；隔离实例实测三类请求全部 200。
- **收益**：custom 工具与图片被**转换保留**，用户功能不丢。
- **代价**：`chat_context.extra.modelConfig` 从 10 字段收窄成 2 字段 —— 这是
  **官方形状**，完整字段仍在顶层 `model_config`；此前那条「两份必须完全相等」
  的测试已改成「extra 只有 2 个键」。
- **代价**：请求体多出 `contents`、`source`、`version` 等字段，body 略大；
  相对 agent 上下文可忽略。
- **已知上限**：`parameters.max_tokens` 用的是客户端传入值（官方用模型目录的
  `max_output_tokens`，网关的模型清单里没有该字段）。若上游要求必须来自模型
  目录，需要把该字段加进 Qoder 模型清单。

## Verification

单测（`cargo test -p ai-gateway-router --lib products::qoder::translate`，22 passed）：

- `system_is_split_out_of_messages_into_its_own_field`；
- `custom_tool_is_converted_to_function_not_dropped`（断言 grammar 定义进了输入说明）；
- `tool_result_image_is_preserved_as_blocks`；
- `assistant_reasoning_content_is_preserved`；
- `base_fields_match_go_spec`（source/version/is_retry/task_id/空 aliyun_user_type、
  chat_context 字符串、contents 块）；
- `tools_are_always_forwarded_in_official_shape`。

全量：core 1307、router lib 1853、server 34、A/B 集成测试、workspace all-targets
全部通过。

实机（隔离实例 57899，`qoder:国服:DeepSeek-Flash`）：

| 请求 | 结果 |
|---|---|
| 简单 | HTTP 200 |
| system + tools + stream | HTTP 200 |
| system + reasoning 历史 + tool_call + tool result + tools（max_tokens 128000） | HTTP 200 |