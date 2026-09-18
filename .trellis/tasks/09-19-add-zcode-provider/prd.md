# prd.md — 接入 ZCode（Z.AI / 智谱 GLM 编码套餐）作为第三个产品

## 1. 背景与目标

参考实现：[TriDefender/zcode-api](https://github.com/TriDefender/zcode-api)
（TypeScript/Bun，Z.AI / 智谱 Bigmodel 的 GLM 编码套餐代理）

**目标**：把 ZCode 作为**第三个产品**接入 workbuddy-switch-gateway，
与已有的 WorkBuddy、Qoder 同等完整度：

- 账号管理页（登录 / 导入 / 额度 / 到期 / 启停 / 删除）
- 凭证管理与令牌刷新
- 网关多产品路由（与 WorkBuddy、Qoder 一起参与选号）
- 全流程实测

## 2. 与已有两个产品的关键差异

| 维度 | WorkBuddy | Qoder | **ZCode（新）** |
|---|---|---|---|
| 鉴权 | Bearer accessToken | COSY 签名（RSA+AES+MD5） | **Bearer `{apiKey}.{secret}`** |
| 令牌刷新 | refreshToken | deviceToken/refresh | **无刷新**（凭证长期有效） |
| 请求格式 | OpenAI | Qoder 私有（嵌套 SSE） | **OpenAI 或 Anthropic（上游两者都收）** |
| 客户端签名 | 无 | COSY（必需） | **无**（实测确认，见 §3） |
| 区域 | 国服 / 国际版 | 国服 / 国际版 | **两个服务商**（Z.AI / 智谱），不是区域 |
| 机器指纹 | 无 | MachineID + Token | **deviceMid + 一批身份头** |

## 3. 最重要的实测结论：**不需要客户端签名**

参考实现里有一整套 **Client Request Signing V4**（Ed25519 签名 + 8 位工作量证明
+ HKDF 派生密钥 + 门控握手），约 600 行密码学逻辑。

但实测表明**直连上游不需要它**。对照实验（`uitest/probe-zcode-signing-control.cjs`，
5 组一次只改一个变量）：

| 组 | 请求头 | 上游响应 |
|---|---|---|
| A | 完全不带 | `401 {"code":"1001","message":"Authentication parameter not received in Header"}` |
| B | 仅 Authorization | `401 {"code":"1000","message":"Authentication Failed"}` |
| C | 身份头 + Authorization | `401 {"code":"1000","message":"Authentication Failed"}` |
| D | 身份头 + **假签名头** | `401 {"code":"1000","message":"Authentication Failed"}` |
| E | 身份头 + **空签名值** | `401 {"code":"1000","message":"Authentication Failed"}` |

**D/E 与 C 的响应完全相同** ⇒ 上游不校验签名头。

另一组探测（`uitest/probe-zcode-signing-required.cjs`）确认三个端点都是
**凭证错误**（而非签名错误），说明请求已到达业务层：

```
api.z.ai/api/coding/paas/v4/chat/completions    → 401 {"code":"1000","message":"Authentication Failed"}
api.z.ai/api/anthropic/v1/messages              → 401 {"error":{"type":"1000"}}
open.bigmodel.cn/api/coding/paas/v4/...         → 401 {"code":"1000","message":"身份验证失败。"}
```

**决策**：**不实现** Client Request Signing V4。理由：

1. 实测不需要（上表）；
2. 参考实现本身是 **fail-open** 的 —— 门控不可达、握手失败、连续两次
   401 都退化成"不签名"。若签名是硬门槛，这个设计早就不可用了；
3. 那 600 行涉及 Ed25519 + PoW + KDF，实现与维护成本高，而收益为零；
4. 若将来上游真的要求，**失败模式是可观测的**（401 `VERIFY_SIGNATURE_*`），
   届时补上即可 —— 我们会在错误分类里识别这个码并给出明确提示。

**保留的后路**：凭证解析里保留 `{apiKey}.{secret}` 两段式拆分（签名需要它），
且错误分类识别 `VERIFY_SIGNATURE_INVALID` / `VERIFY_APIKEY_EXPIRED`，
一旦出现就明确报"上游开始要求客户端签名"而不是笼统的"认证失败"。

## 4. 两个服务商（不是区域）

与 Qoder 的"国服/国际版"不同，ZCode 是**两个服务商**，各有独立域名：

| 服务商 | Anthropic 端点 | OpenAI 端点 | 业务 API host |
|---|---|---|---|
| `zai`（Z.AI） | `https://api.z.ai/api/anthropic` | `https://api.z.ai/api/coding/paas/v4` | `https://api.z.ai` |
| `bigmodel`（智谱） | `https://open.bigmodel.cn/api/anthropic` | `https://open.bigmodel.cn/api/coding/paas/v4` | `https://open.bigmodel.cn` |

**注意**：`auth/resolver.ts` 里 bigmodel 的 host 写的是 `https://bigmodel.cn`，
而 `provider/providers.ts` 写的是 `https://open.bigmodel.cn`。两者不一致 ——
实现时需实测确认（见 implement.md 的开工前检查）。

## 5. 凭证形态

```
zai      →  {apiKey}.{secret}   （两段，secret 必需）
bigmodel →  {apiKey} 或 {apiKey}.{secret}（secret 可选）
```

上游请求头（Anthropic 端点用**双头**，OpenAI 端点用单头）：

```
Anthropic: x-api-key: {cred}  +  Authorization: Bearer {cred}  +  anthropic-version: 2023-06-01
OpenAI:    Authorization: Bearer {cred}
```

## 6. 登录流程（两条路，都要支持）

**方式 A：OAuth 设备流**（参考实现的默认路径）

```
POST {zcode.z.ai}/api/v1/oauth/cli/init   body: {provider}
  → 返回 authorize_url + flow_id
用户在浏览器授权
轮询 GET /api/v1/oauth/cli/poll/{flow_id}
  → 服务端报 ready 时返回 accessToken
```

**方式 B：导入已有凭证**

用户自备的 `{apiKey}.{secret}` 字符串（从 Z.AI 控制台复制）。

**拿到 accessToken 后还要换最终凭证**（`auth/resolver.ts`）：

```
zai:
  1. POST https://api.z.ai/api/auth/z/login  {token}  → access_token（业务令牌）
  2. GET  {host}/api/biz/customer/getCustomerInfo    → orgId + projectId
  3. GET  {host}/api/biz/v1/organization/{org}/projects/{proj}/api_keys
     找不到就 POST 同名创建（name = "zcode-api-key"）
  4. GET  .../api_keys/copy/{apiKey}                 → secretKey
  5. 最终凭证 = {apiKey}.{secretKey}

bigmodel:
  1~3 同上（但 authorization 直接用 accessToken，不经 z/login）
  4. 尽力取 secretKey，取不到就用 apiKey 单段
```

## 7. 额度查询

```
GET {origin}/api/v1/zcode-plan/billing/balance?app_version={v}&platform={p}
GET {origin}/api/v1/zcode-plan/billing/preview?app_version={v}&platform={p}
Authorization: Bearer {jwt}      ← 注意：用 **jwt**（start-plan 令牌），不是 apiKey
```

响应 `data.balances[]`：`show_name` / `remaining_units` / `total_units` /
`used_units` / `unit_type` / `expires_at`（camelCase 与 snake_case 都接受）。

`expires_at` 即**到期时间**，`remaining_units` 即**剩余额度** —— 两者都是
跨产品路由需要的信号（对应 Qoder 的 `rewardExpiresAt` 与 `userQuota`）。

## 8. 模型清单

参考实现硬编码（`provider/models.ts`），共 11 个：

| id | 名称 | 上下文 | 最大输出 | 推理 |
|---|---|---|---|---|
| glm-4.5-air | GLM 4.5 Air | 131072 | 98304 | ✓ |
| glm-4.6 | GLM 4.6 | 200000 | 131072 | ✓ |
| glm-4.6v | GLM 4.6V | 131072 | 32768 | |
| glm-4.7 | GLM 4.7 | 200000 | 131072 | ✓ |
| glm-5 | GLM 5 | 200000 | 64000 | ✓ |
| glm-5-turbo | GLM 5 Turbo | 200000 | 64000 | ✓ |
| glm-5v-turbo | GLM 5V Turbo | 200000 | 131072 | |
| glm-5.1 | GLM 5.1 | 200000 | 64000 | ✓ |
| glm-5.2 | GLM 5.2 | 1000000 | 128000 | ✓ |
| glm-5.3 | GLM 5.3 | 1000000 | 128000 | ✓ |
| glm-5.3-flash | GLM 5.3 Flash | 1000000 | 128000 | ✓ |

**注意**：这是**参考实现硬编码**的清单，可能与上游实际提供的不同。
本项目的既有约定是"不硬编码模型清单"（见 WorkBuddy 的
`model_billing.rs` 注释：判定依据是上游返回，不硬编码）。
故实现时优先**从上游拉取**，硬编码清单仅作兜底。

## 9. 与已有架构的契合点

C5 已建成多产品路由的地基，ZCode 只需接入三处：

1. **凭证加载**（`auth.Product` 标为 `zcode`）—— 与 Qoder 同模式
2. **请求派发**（`server.QoderUpstream` 接口改名/扩展为多产品派发）
3. **成本与到期信号**（`pool.SetCostRate` / `SetCreditsAndExpiry`）

**接口命名问题**：C5b 的接口叫 `QoderUpstream`，现在有第三个产品了，
应重命名为 `ProductUpstream`（纯重命名，行为不变）。

## 10. 验收标准

- [ ] 凭证可导入（`{apiKey}.{secret}` 单段/两段都支持）
- [ ] 两种服务商都能配置与识别
- [ ] 账号页可展示、可启停、可删除
- [ ] 额度与到期能从上游真实拉取
- [ ] 网关能路由到 ZCode 账号（与另两个产品同池）
- [ ] 三个产品都在时，选号不饿死任何一方
- [ ] 全流程实测（真实凭证）

## 11. 不做的事（Out of Scope）

- **客户端签名 V4** —— 实测不需要（§3），且失败模式可观测
- **闲时通道 / 套餐秒抢** —— 参考实现的额外功能，与"账号池 + 路由"无关
- **MCP 托管工具** —— 同上
- **安卓 App / TUI 面板** —— 我们已有自己的界面
- **三种协议转换**（OpenAI ↔ Anthropic ↔ Responses）—— 网关已有，
  且我们**只走上游的 OpenAI 端点**（见 design.md 的取舍）
