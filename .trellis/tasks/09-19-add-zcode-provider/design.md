# design.md — ZCode 产品接入设计

## 1. 决策记录

### D1 只走上游的 **OpenAI 端点**，不做 Anthropic 翻译

上游同时提供 OpenAI 与 Anthropic 两种端点，而我们的网关**内部就是 OpenAI 格式**
（`/v1/chat/completions` 直通，另两个协议入口先转成 OpenAI 再进来）。

走上游 OpenAI 端点 = **零翻译**。走 Anthropic 端点则要双向翻译（请求 + SSE 流），
多出两个易错点却没有任何收益。

**代价**：上游 Anthropic 端点的一些特性（如 `metadata.user_id` 归因）用不上 ——
那些只影响上游侧统计，不影响功能。

### D2 **不实现** Client Request Signing V4

见 prd.md §3 的实测。省掉约 600 行 Ed25519 + PoW + KDF 逻辑。

**后路**：错误分类识别 `VERIFY_SIGNATURE_INVALID` / `VERIFY_APIKEY_EXPIRED`，
一旦出现就报"上游开始要求客户端签名"，而不是笼统的"认证失败"。

### D3 身份头**照发**，但不作为功能依赖

实测（prd.md §3 的 B/C 组）表明身份头不影响认证结果。但仍照发，理由：

- 参考实现的注释反复强调"让代理在**指纹层**与官方客户端不可区分"
  （`identity.ts` 的模块注释：「so the proxy is indistinguishable from the
  official client at the fingerprinting layer」）；
- 发这些头的成本是零，而"被上游风控识别为第三方代理"的代价可能很高。

**但**：代码里**不因缺头而失败** —— 头是可选的，缺失只是少一层伪装。

### D4 凭证形态：**原样存 `{apiKey}.{secret}` 字符串**

不做结构化拆分。理由：
- 上游就是要这个字符串（`credentialString()` 就是把两段拼起来）；
- `bigmodel` 的 secret 是**可选**的（单段也合法），结构化存储要处理"有/无"
  两种形态，而字符串天然支持；
- 签名逻辑（若将来要补）才需要拆分 —— 那时在**使用时**拆即可。

### D5 两个服务商，不是两个区域

`zai` 与 `bigmodel` 是**不同服务商**（不同域名、不同凭证来源），
与 Qoder 的"国服/国际版"（同一产品的两个区域）语义不同。

**但**在数据模型上复用 `auth.Product` + 一个子字段：
`Product = "zcode"`，另存 `Provider = "zai" | "bigmodel"`。
这样池的选号逻辑不需要知道"服务商"这个概念（它只关心成本与到期）。

### D6 额度与到期**从上游真实拉取**，不硬编码模型清单

参考实现硬编码了 11 个模型的清单（`provider/models.ts`）。本项目既有约定是
**不硬编码**（见 `model_billing.rs` 注释：判定依据是上游返回的 credits 倍率）。

故：模型清单优先从上游拉；拉不到时用参考实现的清单兜底（并**标注为兜底**，
让界面能区分"上游真值"与"内置兜底"）。

## 2. 上游协议（Go 实现要点）

### 2.1 端点表

```go
zai:      OpenAI  https://api.z.ai/api/coding/paas/v4
          Biz     https://api.z.ai
bigmodel: OpenAI  https://open.bigmodel.cn/api/coding/paas/v4
          Biz     https://open.bigmodel.cn
```

**实测确认**：`bigmodel.cn` 与 `open.bigmodel.cn` 两个 host **都可达且返回相同**
（参考实现里两处写法不一致，实测证明都对）。实现里用 `open.bigmodel.cn`
（与 `providers.ts` 一致）。

### 2.2 对话请求

```
POST {OpenAIBase}/chat/completions
Authorization: Bearer {apiKey}[.{secret}]
Content-Type: application/json
{标准 OpenAI 请求体，原样透传}
```

**实测**：仅带 `Authorization` 就能到达业务层（回 `1000 Authentication Failed`
而非 `1001 参数未收到`）⇒ 身份头与签名头都不是门槛。

### 2.3 响应形状

上游 OpenAI 端点返回**标准 OpenAI** 响应（流式与非流式都是）——
这是我们选择它的另一个理由：**不需要形状翻译**，可以直接复用既有的
`upstream.Stream` / `upstream.Aggregate`。

（对比 Qoder：嵌套 SSE，必须写翻译层。）

### 2.4 额度查询

```
GET {BizHost}/api/v1/zcode-plan/billing/balance?app_version={v}&platform={p}
Authorization: Bearer {jwt}
```

响应 `data.balances[]`：

```json
{"show_name":"...", "remaining_units":1234, "total_units":5000,
 "used_units":3766, "unit_type":"...", "expires_at":1800000000}
```

字段名 snake_case 与 camelCase 都接受（参考实现两种都读）。

**注意 `jwt` 与 `apiKey` 的区别**：额度接口用的是 OAuth 换来的 **jwt**
（start-plan 令牌），不是 `{apiKey}.{secret}`。这是参考实现明确写的。
若用户只导入了 apiKey（方式 B），则**额度查不到** —— 界面应如实显示"未知"
而不是显示 0（0 会被误读成"额度耗尽"）。

### 2.5 错误码

| code | 含义 | 我们的处理 |
|---|---|---|
| 1001 | 认证参数未收到（完全没带 Authorization） | 配置错误，明确提示 |
| 1000 | 认证失败（凭证无效/过期） | 标记需重新登录 |
| 401 + `VERIFY_SIGNATURE_*` | 上游要求客户端签名 | 明确提示"需补签名实现" |

## 3. 分层与文件

```
go-gateway/internal/zcode/
├── cred.go        凭证解析（{apiKey}.{secret}）+ 落盘
├── provider.go    两个服务商的端点表
├── client.go      对话 / 额度 / 模型清单
├── login.go       OAuth 设备流（cli/init + poll）+ 凭证兑换
├── logincli.go    宿主调用的子命令（qoder-login 的 ZCode 版）
└── *_test.go
```

**为什么独立成包**（与 qoder 同样的理由）：鉴权、端点、凭证形态全都不同，
硬塞进一个客户端会让每个方法都长出产品分支。

## 4. 与既有架构的接入点（C5 已建好地基）

C5 已经把"多产品"做成了可扩展的形状，ZCode 只需接三处：

| 接入点 | 现有代码 | 改动 |
|---|---|---|
| 凭证加载 | `main.go` 的 `if cfg.Pool.MultiProduct` 块 | 加一段 `zcode.LoadDir` |
| 请求派发 | `server.dispatchUpstream()` | 加一个分支 |
| 成本/到期 | `pool.SetCostRate` / `SetCreditsAndExpiry` | 无需改动 |

### 4.1 接口重命名（纯重命名，零行为变化）

C5b 的接口叫 `QoderUpstream` —— 当时只有两个产品，名字还说得过去。
现在有第三个了，继续叫 `QoderUpstream` 会让读代码的人以为只有 Qoder。

重命名为 `ProductUpstream`，**只改名字与注释，不改任何逻辑**。
有专门的验证：重命名前后 `go test ./...` 全绿且**没有测试期望值变化**。

## 5. 宿主侧（Rust）

```
crates/ai-gateway-core/src/modules/
├── zcode_account.rs   账号库（与 qoder_account 同构）
└── zcode_login.rs     登录编排（调 gateway zcode-login 子命令）
```

**与 qoder 的差异**：ZCode 的凭证是**用户可直接复制的字符串**
（`{apiKey}.{secret}`），所以"导入"是**主路径**而不是备选 ——
界面应把"粘贴凭证"放在显眼位置，OAuth 作为另一种方式。

## 6. 界面（C4）

侧边栏加「ZCode 账号」，页面结构与 Qoder 页同构（复用组件），差异：

- **区域选择换成服务商选择**（Z.AI / 智谱）
- **增加"粘贴凭证"入口**（ZCode 的主路径）
- 额度未知时显示"未知"而不是 0

## 7. 风险与回滚

| 风险 | 缓解 |
|---|---|
| 上游要求签名（实测说不需要，但可能变） | 错误分类识别 `VERIFY_*` 并明确报出；届时补签名实现 |
| 模型清单硬编码过期 | 优先从上游拉；兜底清单标注来源 |
| 凭证兑换流程（z/login → org → apikey → secret）步骤多 | 每步都有形状守卫（参考实现的做法），失败给可读原因 |
| 多产品路由改变现有行为 | `pool.multi_product` 开关，关闭即回到单产品（C5 已建） |

**回滚点**：每个子任务一个提交。`pool.multi_product` 关闭时，
ZCode 代码路径完全不被执行 —— 回滚只需关开关，不需要回滚代码。

## 8. 子任务拆分

```
Z1 接口重命名 QoderUpstream → ProductUpstream（纯重命名，零行为变化）
     ↓
Z2 ZCode 后端（凭证 / 服务商 / 额度 / 模型 / 对话）
     ↓
Z3 ZCode 登录（OAuth 设备流 + 凭证兑换）
     ↓
Z4 宿主接入（Rust 命令 + 账号库 + HTTP 路由）
     ↓
Z5 ZCode 账号页
     ↓
Z6 网关派发（接进多产品路由）
     ↓
Z7 全流程实测 + 打包
```

**依赖**：Z2 是地基；Z6 依赖 Z2；Z7 依赖全部。

## 9. 为什么 Z1 放在最前面

它是**纯重命名**，风险最低，但能让后续所有代码读起来正确。
放在最后做的话，Z2~Z6 都要写在一个叫 `QoderUpstream` 的接口上 ——
读代码的人会以为 ZCode 走的是 Qoder 的路径。

**验证方式**：重命名前后跑全量测试，断言"没有任何测试期望值变化"
（与 C5a 的零漂移验证同一手法）。
