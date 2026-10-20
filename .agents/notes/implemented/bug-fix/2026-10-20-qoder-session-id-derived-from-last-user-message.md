# Agent Note: `session_id` 用了最后一条 user 文本——每轮都是新会话

Status: implemented

## Problem

业主现场（2026-10-20）：

> 「我现在使用 qoder:deepseek-falsh 模型报错……上游返回空流……
> 本次已无可切换的账号（其余账号正被在途请求占满）」
>
> 「可是我在官方客户端使用就没问题，是不是有遗漏的地方」

官方客户端的 `qodercli.log` 给出了直接对照（同一账号、同一模型 `dfmodel`）：

```text
[QoderInferRequest] sessionId=8937ddb7-7593-42bc-9ada-25af498dcc29, qoderRequestId=6ed9ec83-…
```

**同一个 `sessionId` 连续数十个请求不变**。而上游按 `session_id` 做会话级
prompt cache 与会话管理。

网关这边的代码则写着：

```rust
let prompt = last_user_text(messages);
base.insert("session_id", json!(derive_session_id(&meta.key, &prompt)));
```

`derive_session_id` 的文档与测试都声称输入是「**首条** user 文本」，实际传的是
**最后一条**。多轮对话每新增一轮，`session_id` 就变一次 ⇒

- 上游缓存**在结构上不可能命中**（每一轮都是新会话）；
- 同一段对话被拆成大量「新会话」，更容易触发上游的会话/频率限制；
- 现象正是「用一会儿就空流，官方客户端同一账号正常」。

## Decision

`session_id` 改用 [`first_user_text`] 派生；`chat_context.text` / `business.name`
继续用最后一条 user 文本（它们的语义确实是「本轮输入」）。两者不再混用同一个值。

**并且把账号 uid 混进派生种子**（`uid\0modelKey`）。原因：网关的空流轮转会
**切换账号**，而 `session_id` 若不区分账号，新账号会继承前一个账号的会话状态
（或被视为不属于它的会话）—— 现场表现为两个账号**连续空流**，而官方客户端
一个会话固定一个账号，完全正常。换号后缓存失效是可接受的代价：正确性优先。

首条 user 文本在整段对话里天然稳定，跨对话又不同，是网关在**无状态**前提下
能做到的最好近似。官方客户端用的是本地会话 UUID，但它能看到完整会话生命周期，
网关只看到一个请求体，拿不到那个 UUID。

## Alternatives considered

- **让客户端传会话 ID（OpenAI `user` 字段 / 自定义头）** — 最强理由是**最准确**：
  客户端本来就知道会话边界。否掉的原因是**跨客户端契约**：DSH / CodeBuddy /
  Claude Code 各有各的写法，没有一个是通用标准；拿不到时仍要回落，于是双路径并存、
  行为随客户端漂移。首条 user 文本是**所有** OpenAI 客户端都有的稳定信号。

- **保持每轮一个 UUID（回到更早的实现）** — 最强理由是**绝不会串会话**。
  否掉的原因是上游缓存命中恒 0：agentic IDE 动辄几万 token 上下文，每轮都全量
  重新处理，额度与延迟都不可接受；而且现场证据表明这会放大空流。

- **只按「首条 user 文本」派生、不混账号 uid** — 最强理由是**换号仍能命中缓存**。
  否掉的原因是它正是现场「两号连续空流」的放大器：一次空流换号后，新账号
  复用旧会话键。与「缓存多命中一点」相比，先把请求修好更重要。

- **每次请求随机 UUID** — 最强理由是**绝不串会话**。否掉原因同上：缓存恒 0。

## Consequences

- **收益**：同一段对话的所有轮次共用一个 `session_id`，上游 prompt cache 能命中。
  实测（隔离实例 57899，同一账号）：第一次请求 `cached_tokens=0`，第二次
  `cached_tokens=2302`，下一轮 `cached_tokens=2304`。
- **收益**：不再把一段对话膨胀成几十个「新会话」，降低触发上游会话限制的概率。
- **收益**：换号后使用**新会话键**，不会继承前一个账号的坏会话。
- **代价**：换号后缓存必然失效（一次全量重算）；这是正确性优先的取舍。
- **代价**：两段**以同一句话开头**、且**同一账号**的不同对话会共用一个
  session_id。这只会让缓存多命中一点，不影响正确性（消息体仍是完整历史）。
- **已知上限**：网关拿不到客户端真实的会话 UUID；首条 user 文本只是近似。
  若客户端会修改第一条用户消息（极少见），session 仍会漂移。

## Verification

单测（`cargo test -p ai-gateway-router --lib products::qoder::translate`）：

- `session_id_is_stable_across_turns` —— 新增一轮 user 后 session_id 不变。
  故障注入：还原成 `last_user_text` 时该条立刻变红。
- `session_id_differs_across_conversations` —— 首条 user 不同则 session_id 不同。
- `session_id_is_scoped_to_account` —— 同一段对话、不同账号 uid ⇒ session_id 不同；
  同一账号重复构造稳定。
- `first_user_text_is_stable_and_handles_parts` —— 字符串/多模态/无 user 三种输入。

实机（隔离实例 57899，未触碰业主 :7864/:3080）：

| 请求 | `cached_tokens` |
|---|---|
| 首次（建立会话） | 0 |
| 同账号重复同一请求 | **2302** |
| 同账号下一轮（追加 user） | **2304** |

⇒ 稳定 session（同账号）确实命中缓存；换号则换 session。

