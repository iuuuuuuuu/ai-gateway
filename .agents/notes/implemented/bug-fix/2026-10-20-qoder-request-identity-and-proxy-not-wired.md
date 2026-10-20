# Agent Note: Qoder 请求身份字段与国际版代理——两处「值存在但没接线」

Status: implemented

## Problem

业主现场（2026-10-20）：

> 「我用 Qoder:DeepSeek-Flash 这个模型，他回答是 qoder，你看看是不是模型没传对，
> 还是提示词的原因」

以及随后补的一句：

> 「还有别忘了 qoder 国际版也需要走代理，我不知道你做了没，记得检查」

### 第一件事：模型名其实传对了

排查的第一个诱惑是「模型没传对」。日志直接否掉了它：

```text
07:33:33 chat uid=019f1772-… product=qoder model=DeepSeek-Flash: 流结束
         （已收 21388 个有效帧 / 4407385 字节，saw_done=true，saw_finish_reason=true）
```

`/usage` 里也有 qoder 归属的 `DeepSeek-Flash` 记录，`x-model-key` 头按
[`CosySession::apply_headers`] 正确带上，模型清单里 `DeepSeek-Flash` 确实存在
（18 个模型之一）。⇒ **模型名从头到尾是对的**，问题不在名字。

### 真因：`model_config` 只发了 3 个字段

我们发的是：

```json
{"key": "<model>", "is_reasoning": false, "source": "system"}
```

而官方客户端 / 参考实现（`jyao0708/qoder2api` 的 `newLegacyTemplate()`）发的是
**10 个字段**：

```json
{"key", "display_name", "model", "format", "is_vl", "is_reasoning",
 "api_key", "url", "source", "max_input_tokens"}
```

上游的 `model_config` 是一个**结构体**：缺字段时它**不报错**，而是回落成
「官方 Qoder CLI 默认身份」。表现就是模型答对了内容、却**自称是 Qoder** ——
上游按默认身份接管了系统提示。这与仓库里已记录过的 `source` 缺失问题
（`translate.rs` 的 `MODEL_SOURCE_SYSTEM` 注释：缺 `source` 时上游不返回
`reasoning_content`）是**同一个结构体上的两个字段**，只是那次修了 `source`
就停了手。

### 第二件事：国际版代理确实从来没走过

`QoderClient::new_with_proxy` 存在，但**只有一个调用点**：
`products/qoder/login.rs` 的登录流程。网关的对话与模型清单走的是
`Dispatch::new()` → `QoderClient::new()` = **真直连**。

⇒ `proxy_scope.intl = true` 对 Qoder **完全无效**。用户在界面上打开了国际版
代理，qoder 通道照样直连 `api3.qoder.sh`。而 `auth.rs` 里早有实测记录：

> 实测 `openapi.qoder.sh`：直连 **10.3 s**，走代理 **1.2 s**（**慢 8.7 倍**）

这正好解释现场「用了一会儿就没办法用了」里的一部分体感 —— 直连慢且常超时，
与下面那篇空流风暴叠加在一起。

## Decision

### `model_config` 发全 10 个字段

新增 `translate::ModelMeta` 承载「上游 key + 清单里的元数据」，由
`build_agent_body(body, &meta)` 构造 `model_config`：

| 字段 | 来源 | 缺失时 |
|---|---|---|
| `key` | `resolve_in_list` 解析出的上游 key | 不可能缺 |
| `display_name` | 清单的 `display_name` | 回落 `key` |
| `model` | 常量 `""` | — |
| `format` | 常量 `"openai"` | — |
| `is_vl` | 清单的 `is_vl` | `false` |
| `is_reasoning` | 清单的 `is_reasoning` | `false` |
| `api_key` / `url` | 常量 `""` | — |
| `source` | 常量 `"system"` | — |
| `max_input_tokens` | 清单的 `max_context_tokens()` | 兜底 `180_000` |

顶层 `model_config` 与 `chat_context.extra.modelConfig` 由**同一个** `to_config()`
产出，测试断言两份**完全相等** —— 它们此前就是各写一份，容易改一处漏一处。

★ 清单里查不到时（`qmodel*` 逃生口 / 清单拉取失败）**不拒绝请求**，退化成
`ModelMeta::key_only`：那两种情况下「拿不到元数据」是正常的，因为拒绝会把一次
网络抖动放大成「模型不可用」。缺的字段由兜底值补上，形状仍是完整的 10 个。

### 国际版代理接进 `Dispatch::new`

新增 `QoderClient::apply_host_proxy()`，判据与 ZCode **逐字同款**
（`products/zcode/client.rs` 的同名方法）：

```text
addr = host_config().proxy.trim()
if !addr.is_empty() && host_config().proxy_intl { 换用代理客户端 }
else { 保持 base_builder() 的 no_proxy() 真直连 }
```

`Dispatch::new()` 构造后立刻调它。设置失败**只打日志、不阻断启动**（退回直连）。

**判据只看 `proxy_scope.intl`，不看单个账号的 `region`**：一个进程只有一对
`reqwest::Client`，两个区域的账号共用它们；按账号切代理要每账号一个 client，
代价远大于收益。国际版是慢的那一侧，故以它为准。这条与 ZCode 的选择一致。

## Alternatives considered

- **按账号 `region` 决定是否走代理** — 它最强的理由是**精确**：国服账号打
  `gateway.qoder.com.cn`，走国内代理反而是绕远路，理论上能让国服更快、国际版
  才走代理，两边都最优。否掉的原因是**一个进程只有一对 client**：`rpc` / `chat`
  两个 `reqwest::Client` 建在 `QoderClient` 上，而 `QoderClient` 被 `Dispatch`
  持有一次、服务所有账号。按区域切就要给每个区域各建一份 client 并按账号选择，
  等于把「客户端」变成「客户端池」—— 连接池、空闲超时、在途名额全都要按区域
  重算。而国服直连本来就不慢（慢的是 `.sh`），收益不抵这层复杂度。

- **在 `record_usage` 里给 `model_config` 补字段** — 它最强的理由是**改一处**：
  计费层已经在按产品分支，顺手补元数据不需要动 `translate` 的签名，测试也不用
  改。否掉的原因是**改错了层**：`model_config` 是**发往上游的请求体**，它的
  正确性由上游决定，与计费无关。放在计费层意味着「没有 usage 帧的请求不会补
  字段」—— 而那恰恰是最需要正确身份的请求（流一开始就空的时候）。

- **只补 `display_name`，其余保持缺失** — 它最强的理由是**最小改动**：现象是
  「自称 qoder」，直觉上补个名字就够了，不必碰 10 个字段。否掉的原因是
  **没有证据支持「只缺这一个」**：我们从未拿到过「只补 display_name 就修好」的
  实测。而 `source` 那次的历史恰好证明这个结构体是**逐字段**影响行为的
  （缺 `source` 只影响 `reasoning_content`，不影响身份）。在没有实测的情况下
  猜「只缺一个」，等于把同一个缺陷留一半 —— 参考实现发 10 个，就发 10 个。

- **在 `Dispatch` 里按区域建两个 `QoderClient`** — 它最强的理由是**为将来的
  分区优化留口子**：真出现「国服被代理拖慢」时可以只调一处。否掉的原因是
  现在没有这个需求，而它会立刻增加两份连接池与两套在途名额，并让
  `apply_host_proxy` 的判据从「看配置」变成「看配置 + 看区域」—— 后者在没有
  实测支撑时是纯负担。

## Consequences

- **收益**：`model_config` 与官方客户端同形，上游不再回落成默认身份；回答不再
  自称 Qoder。**收益的前提是实测确认**（见 Verification），未实测前不得宣称修好。
- **收益**：`proxy_scope.intl` 对 Qoder 真正生效，国际版从直连 10.3 s 降到代理
  1.2 s 量级，并显著减少超时型空流。
- **收益**：`ModelMeta` 让「请求体需要清单里的哪些字段」变成一个**有名字的类型**，
  下一个要往 `model_config` 加字段的人有明确落点，不必再猜。
- **代价**：`translate.rs` 的测试调用点从 `build_agent_body(body, "key")` 全部
  变成 `build_agent_body(body, &ModelMeta::key_only("key"))` —— 签名变长了一点。
  这是刻意的：**只传 key 是不够的**这件事必须体现在类型上，否则下一个调用方
  会再次只传 key。
- **代价**：`Dispatch::new()` 现在有副作用（读宿主配置、可能换 client）。
  这与 ZCode 的既有形态一致，但 `new()` 看起来"应该很轻"——已在方法文档写明。
- **已知上限**：`max_input_tokens` 的兜底值 `180_000` 是**抓包常量**，不是从
  清单推出来的。清单里该模型有值时以清单为准；清单拉取失败时才用它。
  **信号**：若上游开始拒绝「上下文超长」而客户端认为没超，第一件事是查这个值
  是不是兜底值在用 —— 而不是去调客户端。
- **已知上限**：`model_config` 的字段集是**抓包快照**。上游若新增必填字段，
  症状同样是「静默回落默认身份」而不是报错。**信号**：一旦再次出现「模型答对了
  但自称是别的东西」，先 diff `model_config` 与官方客户端，不要再从模型名查起。
- **未证**：本次没有直接证据说明「上游按 `model_config` 完整度决定系统提示」。
  这个因果是**从两条实测事实推出来的**：①模型名与 `x-model-key` 都对；
  ②参考实现发 10 个字段而我们发 3 个。因果链本身要靠实机复测确认。

## Verification

单测（`cargo test -p ai-gateway-router --lib`，1840 passed）：

- `model_config_carries_every_field_the_upstream_expects` —— 断言 10 个字段的
  **值**与**字段总数**，并断言 `chat_context.extra.modelConfig` 与顶层**完全相等**。
- `model_config_falls_back_when_metadata_is_missing` —— key-only 时形状仍是 10 个。
- `model_config_zero_max_tokens_falls_back` —— 清单给 0 时不原样发 0。

**故障注入（已做，证明门禁不是假绿）**：把 `to_config()` 还原成原来的 3 字段
形状，上述 3 条立刻变红（`left: Null, right: 180000` / `left: Null, right: "k"`）。
恢复后重跑全绿。

代理部分**无单测**：`apply_host_proxy` 的效果是「换了一个 `reqwest::Client`」，
而 client 的代理状态没有可断言的公开读取面。判据只能靠实机日志
（`Qoder 走代理：<addr>`）与「国际版首字节延迟」。这是**已知的测试缺口**，
不假装它被覆盖了。

## 实机验证（2026-10-20 08:10–08:15，隔离实例 57891/57892/57893）

独立名 `wb2g-qoder-probe.exe` + 独立端口 + 独立 `auth_dir`/`state_file`，
业主的 `:7864` / `:3080` 全程未动（前后各自检一次）。

### 代理接线：已生效

启动日志新增一行（**修复前不存在**）：

```text
08:10:53 Qoder 走代理：http://127.0.0.1:7890
```

对照：同一份配置下 ZCode / Trae 早就有这一行，Qoder 没有 —— 这就是「没接线」
的直接可见证据。

### 身份：已修好

同一个 `qoder:DeepSeek-Flash`、同一条实例：

| 请求 | 回答 |
|---|---|
| `只回答四个字：你是谁`（无 system） | **我是助手** |
| 带 system「你是 DeepSeek，由深度求索开发」 | **我是 DeepSeek，由深度求索（DeepSeek）公司开发。** |

修复前业主看到的是「他回答是 qoder」。⇒ 上游不再接管身份。

### ★ 一个必须写下来的**误判**：流式响应里的 `model` 恒为 `auto`

流式响应的每个分片都带 `"model":"auto"`，而**非流式**响应是
`"model":"DeepSeek-Flash"`。我一度把它当成新缺陷，实测否掉了：

- 用清单里 `DeepSeek-Flash` 的**真实上游 key**（`dfmodel`）直接发，流里仍是 `auto`；
- 换另一个模型（`Qwen3.8-Flash` → `qfmodel`）同样 `auto`。

⇒ **`model:"auto"` 是上游对流式响应的回显行为**，与「实际用哪个模型」无关。
判据：非流式路径回的是真实模型名，且回答内容正确。**不要**因为流里看到 `auto`
就去改 `model_config` 或 `x-model-key` —— 那是白改。

### 上游真实清单（`qoder-login models`，只读）

顺手记下来，因为「客户端模型名 ≠ 上游 key」这一点极易再次踩坑：

| displayName | key | price_factor |
|---|---|---|
| DeepSeek-Flash | `dfmodel` | **0.0**（免费） |
| Qwen3.8-Flash | `qfmodel` | **0.0**（免费） |
| Qwen3.8-Max | `qmodel_38max` | 0.5 |
| GLM-5.3 | `gmodel` | 0.8 |
| Kimi-K3 | `kmodel_latest` | 1.4 |

⇒ 用户写 `DeepSeek-Flash`（displayName），网关必须解析成 `dfmodel` 才能发对。
这正是 [`resolve_in_list`] 存在的原因，也是它的 `display_name` 归一化分支
必须保留的原因。

### 国际版清单拉取失败：**上游侧限制，不是本次改动引入**

隔离实例上国际版账号（`qoder.sh`）拉模型清单失败：

```text
qoder dispatch uid=01a0c73a…: 拉取模型清单失败…: error sending request for
url (https://api3.qoder.sh/algo/api/v2/model/list?Encode=1)
```

逐层排查（全部可复现）：

| 探测 | 结果 |
|---|---|
| 代理 `127.0.0.1:7890` 存活 | ✅ |
| `api3.qoder.sh` DNS | ✅ CNAME → 阿里云 GA（8.212.20.107 / 47.57.13.70） |
| `curl https://api3.qoder.sh/...` 直连 | **http=000**（连接被重置，0.35 s） |
| 同上走代理 | **http=000** |
| 同上带国际版 Bearer | **http=000** |
| `curl https://gateway.qoder.com.cn/...` 直连（国服对照） | **http=403**（端点通，只是没签名） |

⇒ 国服端点通、国际版端点**连 TLS 层都被重置**，直连与代理**都一样**。
**这不是代理没接上**（代理已生效、积分刷新成功），而是上游对该端点的访问限制。

★ 这个失败**不影响**本次修复的正确性：清单拉不到时的既有语义是「本次不校验
模型名、原样透传」，对话照常成功（实测 HTTP 200、内容正确）。它只让
**国际版账号的倍率查不到**（缓存为空 ⇒ `None` ⇒ 不显示倍率列），
而那正是 `Billing` 三态语义里「不知道」该有的表现 —— 宁可不说，不编造。

**已知上限（新增）**：国际版账号目前拿不到倍率。**信号**：若业主的国际版
qoder 账号在 `/usage` 里长期没有倍率，而国服账号有，那就是这一条，
不要去查 `multiplier_of` 的实现。
