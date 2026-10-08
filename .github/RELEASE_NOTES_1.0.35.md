## AI Gateway 1.0.35 正式版

> 本版**不修新故障**，只把「你报的断流到底断在哪一段」变成**当场可读**。
>
> 上一版（1.0.34）修掉了「回答已完整却报本轮运行失败」那一类。但排查过程中我发现一件更麻烦的事：**网关日志每一行都没有时间**。客户端插件（billion-context）的日志带毫秒时间戳，网关的不带，于是两边永远对不上 —— 只能拿「收到多少字节」去猜，而不是比时间。那一次能定位到「每日维护窗口在毫秒级掐断连接」，靠的全是插件侧的时间戳。
>
> 本版做三件事：① 网关日志**逐行带毫秒时间戳**；② 三个协议入口在**流结束时各留一条凭据**（含收了多少帧、有没有见到 `[DONE]` / `finish_reason`）；③ `/status` 暴露**在途请求数**与**网关版本号**。

---

## 一、本版做了什么

| # | 事项 | 为什么 |
| --- | --- | --- |
| 1 | 网关日志**逐行加本地时间戳**（`2026-10-08 15:42:57.304 +08:00`，毫秒 + 时区） | 跨源对齐的前提。没有它，「断流发生在维护窗口的哪一毫秒」这类问题只能靠字节数猜 |
| 2 | 三个流式入口收尾各记一行：`chat uid=… 流结束（已收 N 个有效帧 / M 字节，saw_done=…，saw_finish_reason=…）` | 过去**流正常结束时一行日志都没有**，于是「网关有没有把这条流收干净」根本无法回答 —— 只能靠有没有 error 帧反推 |
| 3 | `/status` 新增 `in_flight`（此刻在途请求数）与 `version`（网关自身版本） | ① 判「现在重启会不会打断正在服务的流」需要**请求维度**的数，而 `in_flight_full` 是「被顶满名额的账号数」，静默期的 1 个在途流与 27 个账号全满在它上面可能完全一样；② 端口上跑的是哪一版，不必再去猜文件名里的指纹 |

### 1.1 时间戳长什么样

改前（真实片段，**整行没有任何时间**）：

```text
chat uid=8ea1fcbc-… product=workbuddy model=deepseek-v4.1-flash: 上游流读取失败：…
```

改后：

```text
2026-10-08 15:34:45.479 +08:00 chat uid=066a107b-… product=workbuddy model=deepseek-v4.1-flash: 上游未发 [DONE] 就结束，已向客户端补发 error 帧 code=upstream_stream_truncated（已收 2 个有效帧 / 402 字节，saw_done=false）
2026-10-08 15:34:45.479 +08:00 chat uid=066a107b-… product=workbuddy model=deepseek-v4.1-flash: 流结束（已收 2 个有效帧 / 402 字节，saw_done=false，saw_finish_reason=false）
```

毫秒是**必须**的：断流事件在同一秒内成簇出现（实测三条同族事件相隔 129 ms / 770 ms），秒级粒度分不出先后。

### 1.2 覆盖范围

- **stderr 全量**：路由器 crate 里 173 处诊断日志全部走新的 `logline!` 宏（等价于 `eprintln!`，只是前面多一个时间戳）。周期性摘要（Trae 积分刷新、计费周期刷新、配置热重载、平台白名单）也一并改到 stderr，不再散落在 stdout。
- **stdout 一个字没动**：`qoder-login` / `zcode-login` 的子命令 JSON 仍然只走 stdout —— 宿主是用 `serde_json::from_str(stdout.trim())` 直接解析它的，给 stdout 加前缀会**当场打坏**这些子命令。这是本版最硬的一条约束。

---

## 二、真机实测（隔离实例，全程不碰你在用的 `:7864` / `:3080` / `:7890`）

装置：`test-bin/inst-1035-obs` —— 独立数据目录、独立端口 `:17911`、单账号凭证副本、本地假上游 `:17912`（三个模式）。

### 2.1 三种上游收尾形态（OpenAI 流式）

| 假上游形态 | 网关补发的收尾 | 日志（逐字） |
| --- | --- | --- |
| 正常：`finish_reason` + `[DONE]` | 不补，原样透传 | `流结束（已收 3 个有效帧 / 592 字节，saw_done=true，saw_finish_reason=true）` |
| **1.0.34 修的那类**：只有 `finish_reason`、没有 `[DONE]` | 只补一帧 `[DONE]`，**不报错** | `流结束（已收 3 个有效帧 / 592 字节，saw_done=false，saw_finish_reason=true）` |
| 真截断：吐了内容、既无 `finish_reason` 也无 `[DONE]` | 补一帧 error（`code=upstream_stream_truncated`）+ `[DONE]` | `上游未发 [DONE] 就结束，已向客户端补发 error 帧…（已收 2 个有效帧 / 402 字节，saw_done=false）` |

三种形态的响应体都实测抓了原始字节：正常与「只有 finish_reason」的响应体里**只有一个** `data: [DONE]`；真截断的响应体里 error 帧排在 `[DONE]` **之前**（顺序反了客户端会读不到错误）。

### 2.2 另外两条协议

`/v1/messages` → `event: message_start`…；`/v1/responses` → `event: response.created`…，两者都是 `200` 且流式帧正常。三个入口现在都会在收尾各记一行「流结束」。

### 2.3 `/status` 新字段

| 网关 | `version` | `in_flight` | `in_flight_full` |
| --- | --- | --- | --- |
| 隔离实例（1.0.35） | `1.0.35` | `0` | `0` |
| 你正在跑的 `:7864`（1.0.34） | 无此字段 | 无此字段 | `0` |

### 2.4 子命令契约未被破坏

`gateway qoder-login quota --uid <不存在的 uid>` → stdout **恰好一行**合法 JSON：`{"message":"…找不到 uid=… 的凭证","status":"error","uid":"…"}`；参数错误路径（未知子命令）→ stdout 为空、stderr 一行**带时间戳**的用法提示。

---

## 三、测试基线

| 套件 | 结果 |
| --- | --- |
| `cargo test -p ai-gateway-router --lib` | **1688 passed / 0 failed / 1 ignored**（1.0.34 为 1685，本版新增 3 个时间戳单测） |
| `cargo test --workspace` | **3048 passed / 0 failed / 7 ignored**（1.0.34 为 3045 / 6） |

新增单测都在 `logging.rs`：时间戳长度与分段、时区偏移在同一进程内恒定、时间戳单调不减。

---

## 四、刻意**没有**做的事

1. **没有换日志框架**（`tracing` / `log`）。要动 200+ 调用点与依赖闭包，而收益只是「格式更漂亮」；本版用宏拿到同样效果，且**不动 stdout** 这条硬约束天然成立。
2. **没有给 stdout 加时间戳**。子命令 JSON 走 stdout，加了就坏。
3. **没有改任何判据**。1.0.34 的 `saw_finish_reason` 放宽逻辑原样保留（上面 2.1 第二行就是它的实测证据）。
4. **没有动网关之外的任何东西**。客户端插件、宿主、账号库、配置一律未改。

---

## 五、给你的使用提示

- 断流再发生时，**先看网关日志有没有「流结束」那一行**：
  - 有 ⇒ 网关侧把这条流收干净了，断点在网关**之外**（客户端插件 / 本机代理 / 客户端自己取消）；
  - 没有，但有「上游流读取失败」⇒ 上游连接被掐断（TLS 层），日志里会写明是 `peer closed connection without sending TLS close_notify` 还是 `cannot decrypt peer's message`；
  - 两者都没有 ⇒ 网关**根本没收到这条请求**。
- 日志在「网关页 → 查看日志」，或 `%USERPROFILE%\.wb-switch\gateway\gateway_data\gateway.log`（超过 4 MB 自动轮转为 `gateway.log.1`，只保留两份）。
- 本版升级后**需要重启一次应用**（网关是新二进制，旧进程不会自己换成新的）。

---

## 六、追加修复：国服账号「消耗积分」恒为 0（同日，你报的第二个问题）

你在看发布的时候又报了一条：**「workbuddy 国服账号错误的显示积分为 0」**。截图里 15 个国服账号整列都是 0，而 12 个国际版账号正常（130 / 100 / 350 …）。这条也在本版一起修了。

### 6.1 根因：网关的 billing 请求头被国服 WAF 判成「裸 API 客户端」

网关日志里有直接证据 —— **19 个 uid、每个 202 次**同一句：

```text
credit-refresh <uid>: upstream client (http 403):
  {"code":10085,"msg":"请求不合法，如有疑问请联系客服","requestId":"…"}
```

隔离实测（同一个 token、同一个 URL、同一个 body，**只换请求头**）：

| 请求头组合 | 结果 |
| --- | --- |
| 网关原样（Authorization / Accept / Content-Type / X-User-Id / X-Domain） | **403 `code=10085`** |
| 上面 + `X-Client-Platform: web` | **200** |
| 上面 + `Referer` | **200** |
| 上面 + `User-Agent` | **200** |
| 上面 + `Origin`（单独加它**无效**） | 403 |

⇒ 国服计费后台的 WAF 拒绝「看起来像裸 API 客户端」的请求，只要带**任意一个**浏览器/客户端特征头就放行。**不是 token 过期、不是区域错、不是并发限流** —— 同一个 token 连打 12 次全 200，直连和走 `127.0.0.1:7890` 代理都是 200。

### 6.2 为什么宿主那边一直是对的，只有网关错

宿主 `ai-gateway-core/src/modules/credits.rs` 的 `resource_auth_headers` 早就带了 `X-Client-Platform: web` + `Origin` + `Referer`（照 WorkBuddy 用户中心的 Axios 拦截器写的）。而**网关**那条后台巡检（`credit-refresh`）走的是自己那套 `upstream/headers.rs::billing_headers`，没带 —— 同一个接口、两套头表，于是「界面上点刷新能出数、网关后台巡检恒 403」。

### 6.3 改法与实测

`billing_headers` 对齐宿主那套：加 `X-Client-Platform: web`、`Origin`/`Referer`（**跟随账号区域**，国际版带 `codebuddy.cn` 的 Origin 会被按非本区拒绝）、与 chat 一致的 `User-Agent`，并把 `Accept` 对齐成 `application/json, text/plain, */*`。

**隔离实例实测**（真实计费端点，一个国服账号 `3b5d6048`）：

| | 修前 | 修后 |
| --- | --- | --- |
| 网关日志 | `credit-refresh …: upstream client (http 403): {"code":10085…}` | 无 403 |
| `/status` 的 `credits` | `0` | **`7246`** |
| `/status` 的 `expire_day` | （空） | **`2026-10-23`** |

新增 2 个回归单测钉住这些头（少任何一个都会静默退回「积分显示 0」）。
