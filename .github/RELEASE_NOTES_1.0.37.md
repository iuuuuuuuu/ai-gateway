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

## 五、与 1.0.36 的关系

1.0.36 的内容（成本调度三条缝、排空重启、保活兜底、依赖告警）**全部包含**在本版。
1.0.37 只加这一件事。