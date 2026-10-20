# Agent Note: Qoder 的倍率不是没有数据，是没接线——`price_factor` 走产品 trait

Status: implemented

## Problem

业主现场（2026-10-20）：

> 「然后 qoder 的模型还没有倍率，你也看看 一起修复了」

`/usage` 的输出直接确认了：

```json
"modelBilling": {
  "DeepSeek-Flash": [
    {"key":"qoder||?","product":"qoder","hasMultiplier":false,
     "input":212620,"output":24877,"records":5,"total":237497}
  ]
}
```

对比同一份输出里 WorkBuddy 的行：`"key":"workbuddy|cn|0.11","hasMultiplier":true`。
⇒ qoder 的 `hasMultiplier` 恒为 `false`，界面不显示倍率列。

### 代码里是这么写的

`server.rs` 的 `record_usage`：

```text
if product == PRODUCT_WORKBUDDY {
    billing.has_multiplier = true;
    billing.multiplier = billing_multiplier_for(state, region, model);
}
// ← 没有 else。qoder / zcode 保持 Default（has_multiplier = false）
```

### 但数据一直在那儿

`products/qoder/client.rs` 的 `DynamicModel`：

```rust
/// 价格系数。同上（Go `logincli` 的 `priceFactor`）。
#[serde(default)]
pub(crate) price_factor: f64,
```

注释写的是「同上」—— 指的是上一行 `max_input_tokens` 的
「Go 侧由 `logincli` / 探针命令读取，**网关路径不用**」。

而 `capability.rs` 里那条注释把当时的取舍写清楚了：

> 倍率：Qoder 有 `price_factor`（账号级、不分区），但那个值来自账号登录时
> 的模型清单，**不在本函数的可见范围内**（本函数只拿到模型名清单）。
> 故这里 `has_multiplier` 留 `false`…宁可不说，也不编一个可能错的数字。

**「不在可见范围内」是真的，但结论错了。** `price_factor` 一直在
`Dispatch` 的模型清单缓存里 —— `DynamicModel` 的字段，反序列化时就解析进来了，
只是**没有任何人读它**。这不是「没有数据源」，是「数据源没接线」。

仓库里甚至留着它的实测记录（`go-gateway/cmd/qoder-chat-probe/main.go`）：

> `cmd/qoder-models-probe` 实测（只读接口）：
> `qfmodel  Qwen3.8-Flash   price_factor = **0.0000**   ← 14 个模型里唯一免费的`

⇒ 数据真实存在、语义已知（`0.0` = 免费）、连「哪个模型免费」都验过了，
唯一缺的是把它接到计费层。

## Decision

### 倍率查询上移到 `ProductUpstream` trait

```rust
pub trait ProductUpstream {
    …
    /// 该模型在本产品上的计费倍率（`None` = 没有这个概念或未声明）。
    fn model_multiplier(&self, _model: &str) -> Option<f64> { None }
}
```

默认实现返回 `None`，故 ZCode / Trae **一行都不用改**。

`QoderUpstream` 实现它，转发给 `Dispatch::multiplier_of(model)` ——
从**已缓存的**模型清单里查 `price_factor`（同步、不发网络请求）。

### 计费层按产品分流，但不知道产品内部结构

```text
if product == workbuddy      → 能力索引（按区域，现有逻辑不变）
else if let Some(m) = product_multiplier_for(state, product, model)
                             → has_multiplier = true; multiplier = Some(m)
```

新增 `ProductUpstreams::dispatch_by_product(product)`：计费路径手上只有 `product`
字符串，没有 `Auth`，而既有的 `dispatch(&Auth)` 是按账号选上游的。

★ **`has_multiplier = true` 只在真拿到值时才置起。** 没拿到（缓存未命中 /
该产品没有数据源）时保持 `false`，界面不显示倍率列 —— 这比显示「—」更诚实：
一个根本没有倍率概念的产品，显示「不知道」会让用户以为「我们本该知道」。

### `multiplier_of` 的三条匹配口径

1. **缓存未命中返回 `None`，绝不是 `Some(0.0)`** —— `0.0` 在上游的语义是
   **免费**（确定），`None` 是**不知道**。把后者渲染成 0 会让用户放心烧一个
   其实在计费的账号。这正是 `Billing` 三态语义的核心。
2. **与 `resolve_in_list` 同款匹配**：key（大小写不敏感）或归一化后的
   `display_name` 都认。否则用户写 `DeepSeek_Flash` 时「请求成功但倍率查不到」。
3. **跨区域查找**：`price_factor` 不分区（同一模型两个区域同价），而缓存按
   `QoderRegion` 分桶。任一份清单能查到就用它 —— 与 `capability.rs` 对 Qoder
   「region 留空、一个产品一条渠道」的口径一致。

### 启动预热：让 `/v1/models` 的倍率列**首屏**就有值（2026-10-20 补）

倍率读的是 `Dispatch` 的模型清单缓存，而缓存默认只在**第一次对话**时填充。
于是网关刚启动、还没人发过 qoder 对话时，模型路由清单里所有 qoder 倍率都是
空的 —— 用户会以为这个功能没做（也正是业主第二次反馈「这个页面没有显示出
Qoder 的倍率」时的观感之一）。

新增 `ProductUpstream::warm_model_cache`（默认空实现），`QoderUpstream` 用一个
账号的凭证在**启动后台任务**里拉一次清单。只发一次网络请求；失败由
`refresh_models` 打日志，不影响启动，也不改变 `/v1/models` 的 200 契约。

> ⚠ 预热是**优化**：即使它失败，发过一次 qoder 对话之后倍率仍会出现。
> 判据是启动日志里的 `Qoder 模型清单预热完成：…（倍率列首屏可用）`。

## Alternatives considered

- **在 `record_usage` 里直接 `match product` 并读 qoder 的缓存** — 它最强的
  理由是**改动面最小**：一个 `else if` 分支，不动 trait、不动 `ProductUpstreams`，
  也不用新增 `dispatch_by_product`。否掉的原因是**把产品实现细节泄进计费层**：
  `record_usage` 就得知道「qoder 的倍率在 `Dispatch.cache` 里、按 region 分桶、
  字段叫 `price_factor`」。下一个产品（Trae）有倍率时又要再改一次计费层，
  而计费层本该只关心「归属 + 一个数字」。trait 方法让「数据在哪」留在产品内部。

- **把 `price_factor` 提前塞进 `product_models`（宿主透传的清单）** — 它最强的
  理由是**与 WorkBuddy 同构**：既然 WorkBuddy 的倍率走 `product_models` 派生的
  能力索引，qoder 也走同一条路，计费层就只有一个分支。否掉的原因是
  `product_models` 的形状是 `{product: {region: [模型名]}}` —— **只有名字，
  没有值**。要带倍率就得改这个跨进程契约（宿主 → 网关的配置格式），
  而那条链路上有两个必然漂移的副本（`product_models` 与
  `product_models_fallback` 打架的前科已记录在
  [兜底清单不能参与声明竞争](../../implemented/bug-fix/2026-09-22-fallback-model-list-not-a-declaration.md)）。
  网关自己手里已经有一份权威的模型清单（`DynamicModel`），再从宿主传一份
  等于凭空引入第二份状态。

- **让 `multiplier_of` 在缓存未命中时发一次网络请求拉清单** — 它最强的理由是
  **命中率最高**：缓存 10 分钟过期，过期后头几个请求的倍率会缺失，补拉能填上。
  否掉的原因是**它在关键路径上**：`record_usage` 在每个成功响应之后执行，
  用户等的是回答（已经拿到了），此时再发一次上游请求只是为了账单上的一个数字。
  这与 `billing_multiplier_for` 对 WorkBuddy 的既有口径**完全一致**
  （「只用缓存里的真值，不发网络请求」），两条路径不该有不同哲学。

- **`has_multiplier = true` + `multiplier = None`（显示「—」）** — 它最强的理由
  是**信息更完整**：让用户知道「这个产品本来有倍率概念，只是现在没取到」，
  而不是让界面看起来「这个产品没有倍率这回事」。否掉的原因是**当前无法区分
  两种 `None`**：`price_factor` 是 `f64`（非 `Option`），清单里没这一项时
  serde 给 `0.0` —— 与「真免费」不可区分。要区分就得把字段改成 `Option<f64>`，
  那是另一个改动。在能区分之前，`false` 是唯一不说谎的选择。

## Consequences

- **收益**：qoder 的用量行开始显示真实倍率（`DeepSeek-Flash` 等按上游
  `price_factor`），用户终于能判断该烧哪个账号的额度 —— 这是 `/usage` 的
  核心用途。
- **收益**：`Qwen3.8-Flash` 这类 `price_factor == 0.0` 的免费模型会显示为
  **免费**而不是「不知道」，用户可以直接拿它做验证而不消耗积分。
- **收益**：倍率的数据源差异被收进 trait，计费层不再按产品分支写死。
  Trae 将来有倍率时只需实现一个方法。
- **代价**：`ProductUpstream` trait 多一个方法（有默认实现，故不破坏任何
  既有实现），`ProductUpstreams` 多一个 `dispatch_by_product`。
- **代价**：`DynamicModel.price_factor` 的注释从「网关路径不用」改成被使用 ——
  这条改动**推翻了一句既有注释**。已在 `multiplier_of` 的文档里写明来龙去脉，
  避免下一个读 `client.rs` 的人被旧注释误导。
- **已知上限**：`price_factor` 是 `f64` 而非 `Option<f64>`，故「清单里没有这一项」
  与「真免费」在类型上不可区分。当前实现只在**缓存整体命中**时才给出值，
  缓解了这个歧义（缓存命中的清单里一定有该模型），但没有根治。
  **信号**：若上游开始对某些模型不下发 `price_factor`，需要把字段改成
  `Option<f64>` —— 那时 `has_multiplier` 的三态才能完整表达。
- **已知上限**：倍率只影响**展示**，不参与任何计费计算或路由决策。
  这是刻意的（与 WorkBuddy 一致）：倍率是给用户看的参考值，不是网关的计价依据。

## Verification

单测（`cargo test -p ai-gateway-router --lib products::qoder::dispatch`，18 passed）：

- `multiplier_reads_price_factor_from_the_cache` —— 0.11 与 **0.0（免费）**
  都能正确读出，且按上游 key 也认。这条同时钉住「0.0 不等于 None」。
- `multiplier_matches_normalized_display_names` —— `DeepSeek_Flash` /
  `deepseek flash` / 带空格 / 小写四种变体都能查到。
- `multiplier_unknown_is_none_not_zero` —— 空缓存与清单里没有的模型都返回
  `None`，**不是** `Some(0.0)`。
- `multiplier_looks_across_regions` —— 只在 intl 缓存里的模型也能查到。

**故障注入（已做）**：把 `multiplier_of` 改成恒 `return None`，
上述 3 条立刻变红（`left: None, right: Some(0.11)`）。恢复后全绿。

**未覆盖**：`record_usage` 的 `else if` 分支没有单测 —— 它依赖完整的
`AppState`（含 `ProductUpstreams`、账号池、`Stats`），在 `server.rs` 里没有
现成的轻量构造器。这是**已知缺口**：该分支的正确性目前只由
`product_multiplier_for` 的转发 + `multiplier_of` 的单测间接保证。

## 实机验证（2026-10-20 08:13，隔离实例 57893）

同一个国服 qoder 账号，修复前后 `/usage` 的 `modelBilling` 对比：

```text
修复前（业主 :7864 的实测输出）：
  {"key":"qoder||?","product":"qoder","hasMultiplier":false,"records":5}

修复后（隔离实例，发一次 qoder:DeepSeek-Flash 流式请求）：
  {"key":"qoder||0","product":"qoder","hasMultiplier":true,
   "creditMultiplier":0.0,"records":1}
```

三处同时变了，每一处都对得上：

| 字段 | 修复前 | 修复后 | 含义 |
|---|---|---|---|
| `hasMultiplier` | `false` | `true` | 界面开始显示倍率列 |
| `creditMultiplier` | （无） | `0.0` | 真实倍率值 |
| `key` | `qoder\|\|?` | `qoder\|\|0` | `?` = 未知 → `0` = 免费 |

`0.0` 是**正确值**，不是「没取到」：`qoder-login models` 的只读清单显示
`DeepSeek-Flash → key=dfmodel, priceFactor=0.0`（免费）。同一轮里
`Qwen3.8-Flash` 也得到 `mult=0`，其 `priceFactor` 同样是 `0.0`。
⇒ 免费模型显示免费，付费模型会显示各自的值（`GLM-5.3` 0.8、`Kimi-K3` 1.4）。

★ 区分「免费」与「不知道」的判据就在 `key` 末段：`0` vs `?`。
这正是 `Billing` 三态语义在输出上的落点，排障时看这一个字符即可。

**国际版账号拿不到倍率**（清单端点被上游限制，详见
[Qoder 请求身份与代理](../bug-fix/2026-10-20-qoder-request-identity-and-proxy-not-wired.md)
的实机验证一节）。那种情况下 `hasMultiplier` 保持 `false`、界面不显示倍率列 ——
是设计里「不知道就不说」的那一档，不是缺陷。

## 实机验证（2026-10-20 09:06，隔离实例 57899，首屏 / 无对话预热）

启动后**不发任何对话**，直接 `GET /v1/models`：

```text
qoder:DeepSeek-Flash  [{"creditMultiplier":0,"label":"Qoder","product":"qoder"}]
qoder:GLM-5.3         [{"creditMultiplier":0.8,"label":"Qoder","product":"qoder"}]
qoder:Qwen3.8-Flash   [{"creditMultiplier":0,"label":"Qoder","product":"qoder"}]
```

启动日志：

```text
09:06:37.781 Qoder 模型清单预热完成：uid=019f1772… region=国服，14 个模型（倍率列首屏可用）
```

⇒ 「首屏没有倍率」这一档已被预热覆盖；`0` 仍是**确定的免费**，与「未声明」的
`x—` 区分不变。
