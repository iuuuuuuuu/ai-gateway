# AI Gateway 1.0.37

**主题：Qoder 账号的积分余额终于显示真实值。**

---

## 一、现象与根因

### 现象

业主截图：两个 Qoder 账号在「账号池」页的**积分余额列显示 0**，而宿主侧
`~/.wb-switch/qoder/accounts.json` 里明明写着 `credits: 1996` 与 `credits: 1700`。

业主原话：

> qoder这个虽然到期时间查不出来,但是积分余额显示错误,这个是可以查出来的

### 为什么是 0 而不是「—」

账号池页的「积分余额」列取的是**网关 `/status` 的 `credits`**
（`GatewayPage.tsx` 的 `PoolRowMetrics.balance`）。而那个字段只有两条写入路径：

| 写入路径 | 产品闸门 | 对 Qoder 的效果 |
|---|---|---|
| `scheduler::refresh_credits_with_gap` | `a.product_of() != PRODUCT_WORKBUDDY ⇒ continue` | **必被跳过** |
| `products/trae/credits.rs` 采集循环 | 只认 Trae | **不适用** |

⇒ Qoder 账号的 `credits` **从来没有被写过**，恒为 `Entry::default()` 的 0。
而界面渲染判据是 `typeof credits === "number"`，`0` 是数字 ⇒ 显示 **0** 而不是「—」。

### 关键读数（定位过程）

```text
宿主 accounts.json：  credits: 1996 / 1700
网关 /status：        credits: 0    / 0        ← 从来没被写过
上游真实额度：        remaining: 1996 / 1700   ← 完全查得出来
```

业主说的「这个是可以查出来的」是对的 —— `/api/v2/quota/usage` 一直能查，
只是没有任何循环去调它。

---

## 二、修法

新增 `products/qoder/credits.rs`（与 `products/trae/credits.rs` 同形），
在 `bin/gateway.rs` 里接成**第三条**独立定时循环：

```text
每 15 分钟一轮 → 逐个 Qoder 账号 GET /api/v2/quota/usage → 写池的 credits
```

### 为什么另起一条循环

1. **产品闸门**：scheduler 那条打的是 WorkBuddy 端点，Qoder 凭证打过去只会 401
   并在账号记录里留下不属于它的错误；
2. **端点/鉴权完全不同**：Qoder 走 OpenAPI 域、只带 `Bearer DT`**不签名**
   （`client.rs:270-278` 明确写了「两条路径不能混」）；
3. **不占用计费周期槽位**：scheduler 里 `qoder` 那个单槽回调已被 `billing_cycle`
   （`nextResetAt`）占用，塞进去会让两个职责互相阻塞。

### 三条纪律

1. **失败不写池** —— 查不到就跳过这个账号，**绝不**写 0。把「查不到」写成 0
   正是本次要修的 bug 的同类；
2. **只读，不改账号状态** —— 只写 `credits`，不碰 `until` / `breaker_until` / `disabled`；
   查额度失败**绝不**喂熔断器（那是能力探测，不是账号做错了事）；
3. **池里没有 Qoder 账号就不装回调** —— 一个出站请求都不发。

### 到期档位为什么还是「未知」

`fetch_quota` 的 `expiresAt` 是**套餐到期**不是周期重置，且两个账号实测都是 `0`
（上游明确回了 `expiresAt: 0`）。写进 `expire_at` 会污染选号的到期分层，故**不写**。
那是另一件事，见 `userstatus.rs`（`/algo/api/v3/user/status` 的 `nextResetAt`）。
业主说的「到期时间查不出来」与这条一致。

---

## 三、真机实测

### 修好了

```text
Qoder 积分采集已接线：每 15 分钟一轮，每个 Qoder 账号每轮 1 次出站（quota/usage）
Qoder 积分刷新：处理 2 个账号，2 个有更新

/status 的 credits：
  019f…  product=qoder  credits=1996   ← 与上游一致
  01a0…  product=qoder  credits=1700   ← 与上游一致
```

### 修之前（业主正在跑的 1.0.36 实例，同一时刻）

```text
/status 的 credits：
  019f…  credits=0   ← 业主截图上的 0
  01a0…  credits=0
version=1.0.36
```

### 只读性确认

采集循环跑完一轮后，两个凭证文件的 **mtime 与 SHA256 完全未变**：

```text
未改动 ✓  qoder-019f…-…json  hash=7deb05c693816569
未改动 ✓  qoder-01a0…-…json  hash=d4adf9135e7b191e
```

（这一条特意核过：`dispatch::cred_of` 只读文件、`ensure_fingerprint` 的改动
**不落盘** —— 注释里写明「一次查询不该触发磁盘写入」。）

---

## 四、测试基线

**workspace 3075 passed / 0 failed / 7 ignored**（15 个 suite）。

本轮新增 5 个回归测试（`products/qoder/credits.rs`）：

| 测试 | 守什么 |
|---|---|
| `writes_remaining_into_the_pool` | 上游查到的余额必须写进池（业主现场回归） |
| `a_failed_lookup_does_not_write_zero` | **查不到就不写** —— 写 0 正是本 bug 的同类 |
| `only_qoder_accounts_are_processed` | 产品闸门：WorkBuddy 账号不进这条循环 |
| `no_qoder_account_means_no_runner` | 池里没有 Qoder 账号 ⇒ 一个请求都不发 |
| `uid_order_is_stable` | uid 顺序稳定（否则同一轮的错误文案顺序每次不同） |

---


---

## 六、上游把「超上下文预算」报成 11133 —— 归成 `context_length_exceeded`

### 现象

业主现场：`cctv_to_client` 的会话「接手上下文」之后，**无论怎么搞**都报

```text
400 {"code":"model_param_invalid","message":"上游拒绝本次请求的参数（code=11133）：
     the request parameters were rejected by the model provider。这不是账号问题，
     换账号无效；请检查请求参数（如模型名、思考档位、工具定义）后重试。"}
```

### 根因（真上游逐点二分实测）

```text
输入 1,042,398 + max_tokens=16      → 200
输入 1,042,398 + max_tokens=8192    → 400 code=11133
输入 1,042,398 + max_tokens=128000  → 400 code=11133
输入   982,318 + max_tokens=128000  → 400 code=11133
```

同一个 6.03 MB 的 body：**CJK 触发、ASCII 不触发** ⇒ 限的是 token 不是字节。

⇒ 上游预算是 **`输入 + max_tokens ≤ 1,048,576`**，而且**把超限报成 `model_param_invalid`**。

DSH 按「输入 ≤ 1,048,576」估算 ⇒ `1,042,398 + 128,000` 它认为还有余量 ⇒ **不触发压缩**
⇒ 每轮重发同一个超限请求 ⇒ 「怎么搞都是这个错」。

会话读数：`session-5b6d3617` 压缩后载荷 **7.1 MB**（608 条 assistant 消息占 6.0 MB），
DSH 自己报的压力是 666,696 词元（67%），而实际请求已经越界。

### 修法（判据刻意窄）

| 改动 | 作用 |
|---|---|
| `classify.rs` 新增 `is_param_invalid_from_context_overflow` | 11133 **且**文案点名上下文边界才归 `ContextTooLong` |
| `forward.rs` 新增 `budget_exceeded_guess` / `estimate_tokens` / `max_tokens_of` | 发请求前判「估算输入 + max_tokens > 1<<20」，命中则把 11133 归 `ContextTooLong` |
| `server.rs` | 构造 `ForwardCtx` 时算好 `overflow_guess` 传进去 |

`estimate_tokens` 按字符类型分档（CJK ≈ 2 字符/token、其它 ≈ 4），实测校准且**向上取整**
—— 宁可高估：高估只是多压缩一次，低估就是本 bug。门槛：输入须超过上限的 **60%**
才参与判定，小请求即使 `max_tokens` 很大也不误判。

未知模型 / 算不出 ⇒ `None` ⇒ **逐字退回**原来的 11133 行为（不猜）。

### 真机实测

```text
修后：in~1.040M mt=128000 → 400 code=context_length_exceeded  ← 客户端会压缩
      in~1.040M mt=16     → 200（对照：不该误判）
```

验签链路：新 body 的 `detail = "api_error context_length_exceeded Invalid request
parameters…"`，DSH 的 `STRUCTURED_CONTEXT_OVERFLOW` 正则**命中** ⇒ 归类
`CONTEXT_WINDOW_EXCEEDED`（旧 body 的 `model_param_invalid` 不命中）。

---

## 七、随 App 启动与模型清单刷新（同一轮业主报告）

### ① 重启电脑后网关没跟随启动

日志读数：宿主 `11:19:55` 起、网关 `11:19:58` 起（`maybe_autostart` **成功过**），
但用户 `11:22` 看时已经不在，且**没有任何停止日志**。

自启是 setup 里**一次性** spawn，任何「起来了又悄悄没了」都会静默退化成「没自启」。

修法：`maybe_autostart` 每个出口都打日志，并在启动后调 `ensure_gateway_alive`
再确认一次（1.5 秒后查，不在就补拉一次）—— 把静默失败变成日志里有一行。

### ② 点「启动网关」后模型路由清单没更新，切页面才更新

`run()` 此前只 `refresh()`（网关状态），**不重拉模型清单**；而模型清单只在
「挂载 / 信息不完整 / 手点刷新」三条路径加载 ⇒ 网关刚启动时它必然是空的或旧的。

修法：操作后 `Promise.all([refresh(), loadModels(true)])`。`force=true` 是因为
刚起来的网关可能带着**上一个进程**留下的按区域失败负缓存，带缓存拉会拿到空清单。
---

## 八、与 1.0.36 的关系

1.0.36 的内容（成本调度三条缝、排空重启、保活兜底、依赖告警）**全部包含**在本版。
1.0.37 只加这一件事。