# Agent Note: 空流风暴窗口按账号区域分桶——Qoder 的熔断因此永远不触发

Status: implemented

## Problem

[上一轮的阈值夹小](../../implemented/bug-fix/2026-10-20-empty-stream-threshold-permanently-dead-for-small-pools.md)
把 `EMPTY_STREAM_THRESHOLD` 改成「按通道账号数夹小」，期望 qoder（2 个账号）
在第 2 个不同账号空流时熔断。但现场日志显示它**仍然一次都没触发**：

```text
08:36:14 chat uid=019f… product=qoder: 上游返回空流……
08:36:15 chat uid=01a0… product=qoder: 上游返回空流……
08:36:17 chat uid=01a0… product=qoder: 上游返回空流……
08:36:17 chat uid=019f… product=qoder: 上游返回空流……
…（4 轮 × 2 账号，全部以「轮转耗尽」收尾，没有任何「空流风暴熔断中」）
```

### 根因：桶的粒度与轮转范围不一致

`forward.rs` 的窗口键是 `(产品, 账号区域)`：

```text
note(uid, product, acct.region().as_str())
blocked(product, acct.region().as_str(), channel_account_count(product, region))
```

而 qoder 的两个账号分属 `qoder.com.cn`（cn）与 `qoder.sh`（intl），
`qoder:模型名` 的选号却是 **`Region::Any`（整产品轮转）**。
于是两个账号落进**两个各只有 1 个账号的桶**；判据要求「窗口内不同账号数
≥ 阈值」，而阈值下界是 2 —— **两边都永远凑不满**。

上一轮的结论「阈值与账号数挂钩后小池子自动获得正确熔断」因此只对
「同区域多账号」成立；qoder 是「跨区域、每区一个号」，刚好绕过了它。

## Decision

窗口的分桶区域改为**与轮转范围一致**：

```text
产品前缀请求（qoder:…） → 用请求的区域（未指定 = any ⇒ 整产品一个桶）
裸名请求               → 仍用账号区域（WorkBuddy 两区是不同上游，不能互相连坐）
```

实现是 [`emptystream::channel_region`]，`forward.rs` 的 `note` / `blocked` /
`channel_account_count` 三处**共用同一个值**。

## Alternatives considered

- **把 qoder 的阈值直接改成 1（单账号也算）** — 最强理由是**改一行**。
  否掉的原因是它会把「单个账号的一次偶发空流」立刻放大成整条通道停摆；
  而两个账号**分别**空流一次在 qoder 上很常见（上游抖动时各自重试一次），
  停 60 秒的代价远大于多打一次上游。

- **把所有产品的窗口都改成「按产品」不分区域** — 最强理由是**最简单**。
  否掉的原因是 WorkBuddy 的 2026-10-16 现场：国际版账号吃 3012 把国服账号
  一起熔断，用户描述为「国际服路由到然后直接报错，不轮换国内账号，直接死掉」。
  两区是不同上游域名，不能互相连坐。

- **让 `Pool::channel_account_count` 把 qoder 的 `region` 忽略** — 最强理由是
  一处改完全局生效。否掉的原因是它只修正了「分母」而没修正「分子」：
  窗口仍然按账号区域分散记录，`distinct` 永远到不了 2。必须同时改键。

## Consequences

- **收益**：qoder 的「两个账号都空流」现在会在**同一个桶**里被看见，第 2 个
  不同账号空流即熔断；后续请求在窗口内**一个都不发**，返回带恢复时间的说明。
- **收益**：显式区域前缀（`qoder:国服:…`）仍然按区域分桶 —— 那种请求本来
  就只轮转该区域的账号。
- **零变化**：裸名 / WorkBuddy 请求的键与之前完全一致。
- **代价**：`channel_region` 多一层「这个请求是不是产品前缀」的判断；
  判断错会导致熔断过宽或过窄，故有专门的单测钉住四种组合。

## Verification

单测（`cargo test -p ai-gateway-router --lib emptystream`）：

- `product_request_buckets_by_request_region_not_account_region` ——
  `(product, any, cn) → any`、`(product, any, intl) → any`、
  `(product, cn, intl) → cn`、`(bare, any, cn) → cn` 四种组合。
- 既有 `small_pool_reaches_verdict` / `threshold_is_clamped_not_raised` 全绿。

现场判据：重启后若再次出现两账号空流，日志应先出现
`chat: 空流风暴熔断中（… 阈值 2，该通道 2 个账号）`，随后 60 秒内不再有
`product=qoder` 的上游尝试。
