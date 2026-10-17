# Agent Note: Client 类错误的文案不能再报「账号全挂了」

Status: implemented

## Problem

`classify()` 的 `ErrKind::Client` 是**兜底分类**：任何「4xx 但不是已知业务码」的
上游错误都归它（`classify(400, "")` 的既有测试就断言这一点）。而
`friendly_message()` 的 `match` 里**没有 `Client` 分支**，它落进 `_ => String::new()`。

`String::new()` 的含义是「没有更优表述，调用方回退」，于是
`forward.rs` 的轮转耗尽路径退回通用兜底：

```text
all accounts unavailable (cooling/disabled): upstream client (http 400): {"error":{...}}
```

**前半句把原因说反了。** 真实原因是「上游拒绝了这次请求本身」——同一请求体
发给任何账号都会同样失败；而用户读到的却是「所有账号都挂了」，于是他去：

- 重启网关（无效）
- 重新登录账号（无效）
- 换 API 密钥（无效，且把他引向 `clienttext.rs` 记载的那个「密钥明明是对的」死循环）

**正解一个字都没提到**：改请求参数、换模型。

这与 `clienttext.rs` 通篇记载的那类伤害**同型**，只是换了个入口 —— 那里是
「上游 403 被读成密钥无效」，这里是「上游 4xx 被读成账号全挂」。

### 为什么不是照搬参考实现的 `gateway_hint`

调研 `Sliverkiss/workbuddy2api` 的 `gateway_hint`（`internal/upstream/hint.go`）时，
它看起来正是答案：按 `ErrKind` 一对一映射一句英文补充说明。**但它不能照搬**，理由是
本仓的 `message` 已经做完了那件事：

| | 参考实现 | 本仓库 |
|---|---|---|
| `error.message` | **上游 body 原文透传**（它自己的纪律：`message 永远是上游 body 原文透传`） | **已加工成可操作中文**（`friendly_message` 那张表） |
| 需要 `gateway_hint` 吗 | 需要 —— 原文是英文且不可操作，必须补一句「该做什么」 | **不需要** —— `context_too_long_message` 已经写着「请压缩或清空会话（或调小最大输出）后重试」 |

给一个已经把话说清楚的 message 再挂一个 `gateway_hint`，就是**同一件事说两遍**。
那属于假交付：字段存在、测试通过、用户收益为零。

## Decision

给 `friendly_message()` 补上 `ErrKind::Client` 分支，输出**可操作的中文**：

```text
上游拒绝了这次请求本身（不是账号故障）：请检查请求参数、消息内容与模型是否匹配；
换账号或等待都不会改变结果（上游原话：<提炼后的原文>）
```

三条纪律：

1. **可操作** —— 明确否掉两条无效出路（换账号 / 等待），否则用户会去做无用功。
2. **不写状态码** —— `Client` 涵盖 400/422 等多种形态，写死一个数会与真实响应不符。
   （与 `server_status_label` 的理由不同：那里是怕泄漏裸 401/403，这里是怕说谎。）
3. **原文必须过 `client_safe_message` 闸门**再拼进来 —— 见下面「代价」一节，
   这条是测试逼出来的，不是预防性写法。

## Alternatives considered

- **照搬 `gateway_hint`，新增一个与 message 并列的字段** — 它最强的理由是
  **可机读**：字段是结构化的，客户端可以按 `ErrKind` 分支做不同处理（比如上下文
  超长触发自动压缩），而 message 是给人看的自然语言，只能靠正则猜。这个理由本身
  成立，`clienttext.rs` 里那段 `/\b(?:401|403)\b/` 的教训就是「靠 message 正则判定
  有多脆」的实证。否掉的原因是**本仓的 message 已经承担了全部说明职责**，再开一个
  字段等于两套并存的事实来源 —— 而两套来源必然漂移（本仓已有 `product_models` 与
  `product_models_fallback` 两套清单打架的完整前科）。真要机读，应该先把 message
  降级为「原文透传」，那是**另一个量级的重构**，不是这次的范围。

- **让 `Client` 也回退到上游原文（不动 `friendly_message`）** — 它最强的理由是
  **不撒谎**：原文是上游自己说的，网关不加工就不会引入错误归因，也不会像本次实现
  那样差点把 `"code":"403"` 拼进去。否掉的原因是原文**恰恰不可操作** ——
  `{"error":{"code":"invalid_param","message":"..."}}` 对用户没有任何指向性，
  他会继续猜；而且原文里常带裸 401/403，直接透出就是 `clienttext.rs` 那个病的复发。

- **只在 `forward.rs` 的轮转耗尽处改兜底文案，不动 `friendly_message`** — 它最强的
  理由是**改动面最小**：只碰一处，不碰共享的纯函数，风险最低。否掉的原因是
  `friendly_message` 是**导出的纯函数**，`forward.rs` 不是它唯一的调用方；
  在调用方修补意味着下一个调用方又要修一遍，而且纯函数「返回空串」这个契约会一直
  留着这个坑。

## Consequences

- **收益**：`Client` 类错误（上游 4xx 兜底）不再被说成「账号全挂了」，用户被指向
  「改请求」而不是「重启/重登/换密钥」。
- **收益**：`friendly_message` 的覆盖缺口从 3 个降到 2 个（剩 `None` 与
  `ContextTooLong`/`RequestRejected`，后两者在 `request_side_failure` 里已单独处理，
  `None` 表示成功，都**不该**有文案）。
- **代价（真实发生过）**：第一版实现直接把 `envelope_display_message` 的结果拼进文案，
  **被测试抓出**上游原文里的 `"code":"403"` 会被原样透出 ⇒ 触发 DSH 的 AUTH 误判 ⇒
  反而制造了原本没有的伤害。修法是让原文也过 `client_safe_message`。
  **信号**：若将来有人「优化」掉那个闸门调用，`client_kind_never_leaks_bare_auth_codes`
  会立刻变红 —— 那条测试守的就是这件事。
- **代价**：文案变长了（中文说明 + 原文）。这是有意的取舍：可读性优先于简洁，
  原文对排查不可替代。
- **已知上限**：`Client` 是兜底分类，它**不区分**「参数错」与「内容被拒」与
  「模型不存在」—— 那三种在 `classify()` 里各有专门分支，走不到这里。所以本分支的
  文案只能是通用表述，不能更精确。**若要更精确，该做的是让 `classify()` 多认出
  几种形态，而不是在文案上继续雕花。**

## Verification

新增 3 条测试（`crates/ai-gateway-router/src/upstream/classify.rs`）：

| 测试 | 守什么 |
|---|---|
| `client_kind_says_request_side_not_account_side` | 不得落进空串兜底；必须指向请求侧；必须否掉换账号/等待 |
| `client_kind_never_leaks_bare_auth_codes` | 4 种夹带上游原文的形态都不得让文案含裸 401/403 |
| `client_kind_keeps_upstream_original` | 上游原文必须保留（排查线索） |

**故障注入**（已做，不是宣称）：把 `Client` 分支整段删除后重跑，
`client_kind_says_request_side_not_account_side` 与 `client_kind_keeps_upstream_original`
**双双 FAILED**；恢复后复绿。

⚠ 第三条在故障注入下仍通过是**正确的**：空串不含裸 403，确实「无泄漏」。
判据本身没错，是它守的性质在空实现下被平凡满足。

回归：

- `cargo test -p ai-gateway-router --lib` → **1805 passed / 0 failed / 1 ignored**
  （基线 1802 + 本次 3 条）
- `cargo test -p ai-gateway-core` → **1301 passed / 0 failed / 4 ignored**（与基线一致）
