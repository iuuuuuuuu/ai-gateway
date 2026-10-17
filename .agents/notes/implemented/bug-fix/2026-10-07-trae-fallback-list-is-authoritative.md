# Agent Note: 例外：Trae 的兜底清单是权威的，可以硬排除

Status: implemented

## Problem

原 `docs/DECISIONS.md` D08（从 DECISIONS.md 迁移）。

上一篇（`2026-09-22-fallback-model-list-not-a-declaration.md`）把「静态兜底清单没命中」
从「确定没有」降级成「**猜测**」（`guess_excluded`）之后，出现了一个反向缺陷：

**27 个 WorkBuddy 号 + 1 个 Trae 号一起进了抽签池** ⇒ 约 1/32 ≈ **3%** 的请求
抽到那个 Trae 号，而 Trae 上游确实没有 `space-bunny` ⇒ 400。

即：把「猜测」这一档放宽是对的，但**对所有产品一刀切地放宽**是错的 ——
有的产品，它的兜底清单根本不是猜测。

## Decision

新增 `product_fallback_authoritative` 标记「哪些产品的兜底清单是权威的」。
**只有 Trae 进这个集合**（注入点 `crates/ai-gateway-router/src/bin/gateway.rs`）。
对权威产品，兜底清单**可以参与硬排除**。

### 为什么 Trae 可以例外

它的兜底清单**不是猜测** —— 派生自 `products/trae/dispatch.rs` 的 `ALIASES` 表
（**58 条**客户端名 → **18 个**上游 `config_name`），仓库文档原文称它是
Trae 模型名的「**唯一**真相」。

⇒ 对 Trae 而言它就是**确定**；而 WorkBuddy 的静态表是 **2026-09-18 的快照**，
才是真正的猜测。两者同叫「兜底清单」，可信度完全不同。

### 比结论更值钱的教训

两篇合起来是一条完整口径：

> 「**猜测 vs 权威**」不是一个全局开关，而是**逐产品**的属性。

把两者混为一谈，两个方向都会错：

| 混法 | 后果 |
|---|---|
| 混成「都权威」 | 上一篇的抢号：静态快照获得声明权，压过真实查询 |
| 混成「都猜测」 | 本篇的 3% 抽错平台：权威表被降级，抽到没有该模型的产品 |

## Alternatives considered

- **把「权威性」做成全局布尔** — 它最强的理由是**模型最简单**：一个开关、
  一个分支，不需要维护一份「哪些产品权威」的清单，也不会漏标新产品。
  否掉的原因是它就是上表两种错的合体：开关为 true 时 WorkBuddy 的快照获得声明权
  （抢号），为 false 时 Trae 的权威表被降级（3% 抽错）。一个全局布尔无法同时表达
  两个产品各自的真实可信度。

## Consequences

- **收益**：Trae 的 3% 抽错消失，同时保住上一篇对 WorkBuddy 静态表的降级。
- **收益**：「权威性是逐产品属性」这条口径被写下来，新产品接入时会先问
  「它的兜底清单是快照还是真源」。
- **代价与已知上限**：多一份需要维护的集合 —— 新产品如果**其实**是权威的却没进集合，
  会复现 3% 抽错；如果**其实是猜测**却进了集合，会复现抢号。
  **信号**：若某个产品出现「偶发（约 1/N）路由到没有该模型的产品」，
  或「该产品的模型明明存在却选不到」，先查它的权威性标记对不对。
- **代价**：`ALIASES` 表的权威资格**会随时间衰减** —— 它是与上游对齐时的快照，
  上游改模型名后它就退化成猜测，而集合里仍标着权威。

## Verification

- 标记注入点：`crates/ai-gateway-router/src/bin/gateway.rs` 的
  `product_fallback_authoritative`。
- `ALIASES` 表位置：`crates/ai-gateway-router/src/products/trae/dispatch.rs`。
- 回归判据：Trae 号不应被抽中提供 `space-bunny`；WorkBuddy 的静态表仍不得参与声明。
- **重访条件**：Trae 的 `ALIASES` 表不再被上游真实清单覆盖 / 校对时，
  它的「权威」资格要重新评估 —— 判据是「这张表最后一次与上游对齐是什么时候」。
