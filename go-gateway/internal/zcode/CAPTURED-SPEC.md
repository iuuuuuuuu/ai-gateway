# ZCode 对话请求权威规格（2026-09-20 抓包实测）

> 本文件由 `uitest/extract-zcode-spec.cjs` 从 Reqable 抓到的**成功请求**生成。
> 这是**唯一可信**的规格来源 —— 此前所有关于该通道的结论都是推断，且多次出错。

## 请求

```
POST https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages
```

## 请求头（逐字，共 23 个）

| 头 | 值 |
|---|---|
| `anthropic-beta` | `mid-conversation-system-2026-04-07` |
| `anthropic-version` | `2023-06-01` |
| `authorization` | `Bearer <jwt>` |
| `content-type` | `application/json` |
| `http-referer` | `https://zcode.z.ai` |
| `user-agent` | `ZCode/3.14.0 ai-sdk/provider-utils/4.0.27 runtime/node.js/24` |
| `x-aliyun-captcha-verify-param` | `<base64 json>` |
| `x-aliyun-captcha-verify-region` | `cn` |
| `x-api-key` | `<jwt>` |
| `x-client-language` | `zh-CN` |
| `x-client-timezone` | `Asia/Shanghai` |
| `x-os-category` | `windows` |
| `x-os-version` | `10.0.19045` |
| `x-platform` | `win32-x64` |
| `x-query-id` | `01a0bc8b-d86e-7e99-9808-73c0d0a52642` |
| `x-release-channel` | `production` |
| `x-request-id` | `04f0cee5-ab55-43f8-9758-0866a228820e` |
| `x-session-id` | `8fc6b5b0-fb13-4801-b1de-988f41d14eed` |
| `x-title` | `Z Code@electron` |
| `x-zcode-agent` | `glm` |
| `x-zcode-app-version` | `3.14.0` |
| `x-zcode-session-type` | `main` |
| `x-zcode-trace-id` | `65638a21-a6ea-43ae-8efd-070baf05a997` |

### 与我们实现的差异（**逐条列出，这是要改的地方**）

| 头 | 官方 | 我们 | 影响 |
|---|---|---|---|
| `x-api-key` | **发**（与 authorization 同值） | **不发** | Anthropic 协议要求；可能影响鉴权 |
| `x-query-id` | **发** | **刻意不发** | 我们 identity.go 的注释说"发了触发 3012" —— **实测官方在发** |
| `x-session-id` | **发** | **刻意不发** | 同上 |
| `anthropic-beta` | **发** | 不发 | 协议扩展声明 |
| `user-agent` | `ZCode/3.14.0 ai-sdk/provider-utils/4.0.27 runtime/node.js/24` | 我们发别值 | 官方这串带 **runtime/node.js/24** |
| `x-device-mid` | **对话请求里没有**（只在 configs/balance 里有） | — | 注意差异 |
| `Authorization` | `Bearer {jwt}` | ✅ 一致 | — |
| `x-zcode-session-type` | `main` | ✅ 一致 | — |
| `x-aliyun-captcha-verify-region` | `cn` | ✅ 一致 | — |

⚠ **最重要的一条**：我们的 `identity.go` 里有一条注释（引自某参考实现）：

> 「start-plan（JWT 通道）只发 x-request-id / x-zcode-session-type /
>   x-zcode-trace-id 三个头，**不发** x-query-id / x-session-id。误发会触发 3012」

**但实测官方客户端在发这两个头。** 那条注释的结论**与实测矛盾** ——
要么它针对的是另一个版本/通道，要么它本身就是错的。
在拿到更多证据前，**不应再把它当作约束**。

## 请求体（Anthropic Messages 协议）

```json
{
  "model": "GLM-5.3-Flash",
  "max_tokens": 128000,
  "metadata": { "user_id": "{\"device_id\":\"...\",\"account_uuid\":\"\",\"session_id\":\"...\"}" },
  "system": [ { "type": "text", "text": "...", "cache_control": { "type": "ephemeral" } } ],
  "messages": [ { "role": "user", "content": [ { "type": "text", "text": "..." } ] } ],
  "tools": [ ... ],
  "tool_choice": { "type": "auto" },
  "stream": true,
  "thinking": { "type": "enabled" },
  "output_config": { "effort": "low" }
}
```

要点：
- `system` 是**数组**（Anthropic 形态，每项可带 `cache_control`）
- `messages` 里可以有 **`role: "system"`**（Anthropic 的 mid-conversation system）
- `thinking` / `output_config.effort` 是**扩展字段**
- 不是 OpenAI 的 `choices` 形态

## 响应（SSE，Anthropic 事件流）

```
event: message_start          data: {"type":"message_start","message":{...}}
event: ping                   data: {"type":"ping"}
event: content_block_start    data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}
event: content_block_delta    data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"我叫"}}
...
event: content_block_stop     data: {"type":"content_block_stop","index":0}
event: message_delta          data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{...}}
event: message_stop           data: {"type":"message_stop"}
```

⇒ 网关内部是 **OpenAI 格式**，故需要 **双向翻译层**（请求 + SSE 流）。

## captcha param：格式**已验证一致**

我们的求解器输出与官方客户端的 param **逐字段同构**：

```
{"certifyId":"<10 字符>","sceneId":"11xygtvd","isSign":true,"securityToken":"<固定前缀>+<25 字符变量>+<固定后缀>"}
```

| | 长度 | 结构 |
|---|---|---|
| 我们的 | 284 | ✅ 同构 |
| 官方 | 280 | ✅ 同构 |

**故 3007 不是格式问题。** 剩余可疑点（**未确证**）：
1. happy-dom 环境的浏览器指纹与真实 Chromium 差异 → aliyun 服务端判定 token 可疑
2. param 的时效极短，或与生成时的上下文绑定

## 权益结构（billing/balance 实测）

```json
{
  "plans": [{
    "plan_id": "zcode-v3-start-plan-0817",
    "name": "ZCode Start Plan",
    "status": "active",
    "ends_at": 1790179199,
    "entitlements": [
      { "show_name": "GLM-5.3",       "capabilities": ["model:glm-5.3"],       "grant_units": 3000000, "period": "daily" },
      { "show_name": "GLM-5.3-Flash", "capabilities": ["model:glm-5.3-flash"], "grant_units": 5000000, "period": "daily" }
    ]
  }],
  "balances": [
    { "show_name": "GLM-5.3",       "total_units": 3000000, "used_units": 0,       "remaining_units": 3000000 },
    { "show_name": "GLM-5.3-Flash", "total_units": 5000000, "used_units": 351022,  "remaining_units": 4648978 }
  ]
}
```

⇒ 权益是 **`period: "daily"`（每日重置）**，且 `capabilities` 里写着**允许的模型**。
这正是「界面显示模型清单」应该依据的字段（比 `/models` 目录准确）。

## 官方 provider 端点表（从 cdn-zcode.z.ai 权威配置取）

| providerId | baseUrl |
|---|---|
| `account:zai-start-plan` | `https://zcode.z.ai/api/v1/zcode-plan/anthropic` |
| `account:bigmodel-start-plan` | `https://zcode.z.ai/api/v1/zcode-plan/anthropic` |
| `account:zai-individual-coding-plan` | `https://api.z.ai/api/anthropic` |
| `account:bigmodel-individual-coding-plan` | `https://open.bigmodel.cn/api/anthropic` |
| `account:*-offpeak-idle-plan` | `https://zcode.z.ai/api/v1/off-peak/anthropic` |

配置来源：`https://cdn-zcode.z.ai/zcode/config/zcode-builtin-23.json`（我们此前从未取过）
