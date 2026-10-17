# Agent Note: 3012 熔断按（产品, 区域）隔离，官方身份头与会话头族补回

Status: implemented

## Problem

原 `docs/DECISIONS.md` D18（从 DECISIONS.md 迁移）。

业主现场（原话）：

> 「使用 deepseek-v4.1-flash 模型，**国际服路由到然后直接报错，不轮换国内账号，直接死掉**」
> 「我发现**国服也会出现这个情况**」

报错原文是**我们自己**的措辞（出自 `unusual.rs` 的 `unusual_activity_message`），
即「网关主动拒发 + 熔断倒计时」，**不是上游直接拒绝**。

核实出两个独立缺陷（业主拍板「两条都修」）：

**缺陷①：熔断粒度错了。** 3012 熔断按**产品**做键，而国服账号与国际版账号
**同属 `workbuddy` 产品** ⇒ 国际版一侧抖动会把整个 `workbuddy` 熔断 90 秒，
连主力国服通道一起挡住，一个请求都不发。这正是「不轮换国内账号，直接死掉」的成因。

**缺陷②：官方身份头与会话头族在 Rust 重写时整体丢失。** 国际版路径本该发一套
官方客户端身份头与会话头族，实际一个都没发，UA 还恒为国服。缺身份头不会报错，
只会 403；缺会话头族则被上游渠道校验判成「不是官方客户端发出的」。

两个缺陷叠加的表现相同（国际版请求失败），但成因、修法、验证面完全不同，
因此分节记录。不修缺陷①，国服主力通道会持续被国际版的偶发抖动拖停；
不修缺陷②，国际版请求即便不熔断也过不了上游身份/渠道校验。

## Decision

### 熔断键改为 `(产品, 区域)`

`forward.rs` 的 3012 熔断原先是 `ctx.unusual.for_product(acct.product_of())`。
现改为 `(产品, 区域)` 做键：新增 `UnusualBreakers::for_scope`，`for_product`
保留为便捷入口（区域传空）。

**为什么区域这一维是必须的**：国服与国际版打的是**两个完全不同的上游**
（`codebuddy.cn` vs `workbuddy.ai`）—— 一侧风控**推不出**另一侧也在风控。
把两者绑在一个熔断器上，等于用一次国际版抖动去停掉主力国服通道。

**为什么产品这一维必须保留**：`zcode` 的风控不得阻断 `qoder`/`workbuddy`
（既有回归 `unusual_activity_isolated_by_product`，是另一条现场）。

**与空流熔断的一致性**：本文件的**空流风暴熔断**一直就是按 `(产品, 区域)` 做键的
（`forward.rs` 的 `empty_stream.blocked`）—— 3012 这条只是**没跟上**，本次对齐。
即这是**消除一处内部不一致**，不是发明新口径。

### 官方身份头：重写时整体丢失

对照 Go 参照实现 `go-gateway/internal/upstream/headers.go`，国际版路径**本该发**：

| 头 | 值 |
|---|---|
| `User-Agent` | `WorkBuddy/5.6.2 WorkBuddy/5.6.2 CLI/2.147.0`（国服仍是 `CLI/2.63.2 CodeBuddy/2.63.2`） |
| `X-IDE-Type` / `X-IDE-Name` / `X-IDE-Version` | `WorkBuddy` / `WorkBuddy` / `5.6.2` |
| `X-Agent-Intent` / `X-Agent-Purpose` / `X-Agent-Type` | `craft` / `conversation` / `main` |
| `x-codebuddy-request` | `1` |

而 Rust 的 `chat_headers`（主对话链路，`client.rs` 的 `chat_stream` 调用）：
**这套头一个都没发，UA 还恒为国服**。

**一手证据不是推断**：Go 侧有**专门的测试文件** `intl_headers_test.go` 逐字钉住这些值，
文件头记录着业主 2026-09-27 的现场「**国际版不能使用**」与实测对照：

```text
官方国际版客户端（WorkBuddyAI.exe 5.6.2） → HTTP 200，正常流式
网关（国服 UA）                          → HTTP 403 code=11140 request illegal
```

★ **这套头与它的测试一起在重写时丢失，所以没人发现** —— 文件头自己写着这条教训：
「**UA 错了不会报错，只会 403**，而 403 在客户端界面上还会被渲染成『API 密钥无效』，
排查方向会被彻底带偏」。

### 会话头族：四个头，确定性派生

**缺会话头族的后果**（对应业主给的参考方案
[workbuddy2api-hub#381](https://github.com/ardeyouxipianyi/workbuddy2api-hub/pull/381)）：
上游**渠道校验**回 `400 / code 11-128` `conversation id missing`，
报文是「这次调用没通过渠道校验，可能不是官方客户端发出的」。
官方三套身份里 **VSCode / CLI 一直带**，**只有桌面端一个都没带** ——
而那正是 WorkBuddy 账号默认的身份。

现补 `X-Conversation-ID` / `X-Conversation-Request-ID` /
`X-Conversation-Message-ID` / `X-Request-ID`，**后两者同值**（与参考实现一致）。
`X-Conversation-ID` 由会话键**确定性派生**（`uuid5(NAMESPACE_URL, seed)`，大写字面量）——
官方客户端**整段会话复用同一个** id；每次新造会让上游看成「一串互不相干的单轮请求」。
派生种子取 `sess_key`（`session::extract_key`），它在**整轮换号重试里保持不变**，
正是会话头族要的语义。

### ⚠ 会话头族不分区域，身份头只在国际版发

会话头族**不分区域**（参考实现里三套身份、两个区域都发），身份头**只在国际版发**。
国服多发国际版专属头会改变它的请求画像，而国服是主力通道 ——
有测试 `cn_has_no_intl_only_headers` 专门防「修过头」（Go 侧同款测试防的也是这个）。

## Alternatives considered

- **熔断键只按区域、不保留产品维** — 它最强的理由是最小化键空间：既然国服与国际版
  打的是两个完全不同的上游，区域似乎已经能表达「两套互不相干的上游」，
  多带一维只会让键更难推理。否掉的原因是产品维挡的是另一类事故：
  `zcode` 的风控不得阻断 `qoder`/`workbuddy`，这条有既有回归
  `unusual_activity_isolated_by_product` 钉着，且是另一条现场。区域维**不能**替代
  产品维 —— 它们各自隔离的是一组不同的上游。

- **维持「只按产品」并靠更短的熔断窗口缓解** — 它最强的理由是改动面最小：不动
  键的类型、不动调用点，只调一个时长参数，回归风险接近于零。否掉的原因是它治的是
  症状：只要键里没有区域，国际版抖动就必然连带停掉国服，缩短窗口只是把「停 90 秒」
  变成「停得短一点」，主力通道仍会被拖停。

- **身份头对国服与国际版都发（「反正多发几个头不会错」）** — 它最强的理由是消除
  分流判断：一条路径、一套头，不会出现「某个区域忘了发」这类漏配，也不用维护
  `cn_has_no_intl_only_headers` 这样的反向门禁。否掉的原因是国服是**主力通道**，
  多发国际版专属头会改变它的请求画像；参考实现里两个区域的请求形态本来就不同。
  这条恰好是「修过头」的典型，因此有专门的门禁防它。

- **`X-Conversation-ID` 每次请求新造** — 它最强的理由是实现最简单：不用找种子、
  不用引 uuid5、也不用保证「整轮换号重试里不变」，每个请求一个随机 id 就行。
  否掉的原因是官方客户端**整段会话复用同一个** id；每次新造会让上游把一段会话看成
  「一串互不相干的单轮请求」，会话头族就失去意义。

- **只靠单测断言 `HeaderMap`，不起线级测试** — 它最强的理由是快、稳、无网络、
  无端口占用，且能覆盖每一条头的取值。否掉的原因是它证明不了「HTTP 往返之后
  上游确实收到了」：构造 HeaderMap 与真的把它写上线是两件事，中间还有序列化、
  中间件、以及「调用点忘了传种子」这类接线缺陷。本仓库对此有明确约定
  （见 `ab_devicetoken.rs` 文件头），因此另起 `tests/ab_conversation_headers.rs`。

## Consequences

- **收益**：一次国际版抖动不再停掉主力国服通道 —— 熔断的作用域与「打的是哪个上游」
  对齐，这是语义上的修正，不是阈值调整。
- **收益**：国际版请求恢复官方客户端形态。身份头让请求不再被上游按
  `403 code=11140 request illegal` 拒掉；会话头族让请求能过渠道校验，
  不再回 `400 / code 11-128 conversation id missing`。
- **收益**：3012 熔断与空流熔断现在用同一个键口径，内部不一致消除 ——
  后人改其中一条时不会再撞上「另一条为什么不一样」。
- **代价与已知上限**：多了一套必须随客户端版本走的常量（`5.6.2` / `2.147.0` /
  `2.63.2`）。客户端升级后这些值会陈旧，而上游对陈旧 UA 的反馈是 403 而非报错 ——
  即**失败是静默的**，只能靠测试与日志发现。
- **代价与已知上限**：会话头族的确定性派生依赖 `sess_key` 在整轮换号重试里稳定。
  任何改变会话键提取口径的改动都会连带影响这里，而表现是「上游把会话看成单轮请求」，
  同样不报错。
- **信号（翻案条件）**：若实测发现国服其实**也**需要那套国际版身份头
  （即「不发就不行」），则本条的「只在国际版发」应被推翻 —— 但必须**有实测数据**，
  不能凭「国际版有、国服大概也要」外推。

## Verification

**三次故障注入，每次都必须变红**：

| 注入 | 应红的门禁 |
|---|---|
| UA 退回恒为国服 | `intl_uses_workbuddy_client_ua`、`resource_headers_ua_follows_region` |
| 国际版身份头不发 | `intl_sends_ide_and_agent_headers` |
| 熔断键退回只按产品 | `unusual_activity_isolated_by_region_within_a_product` |
| 会话头族不发 | `conversation_headers_are_always_present` 等 4 条 |
| 会话 id 丢掉确定性 | `conversation_uuid_matches_uuid5_namespace_url` 等 2 条 |
| 线级：`chat_stream` 不传种子 | `conversation_id_is_stable_across_requests_on_the_wire` |

★ **最后一层是「线级」测试**（`tests/ab_conversation_headers.rs`）：起**真** axum 服务、
发**真**请求、读它**实际收到**的头。单测只看 `HeaderMap`，证明不了「HTTP 往返之后
上游确实收到了」—— 本仓库对此有明确约定（见 `ab_devicetoken.rs` 文件头）。

防修过头的反向门禁：`cn_has_no_intl_only_headers`（国服不得出现国际版专属头）。
