## AI Gateway 1.0.34 正式版

> 本版修你报的那一个错：**对话输出完之后就中断**（你说「这是我为什么回退版本的原因」）。
>
> 根因不在你的网络，也不在上游「答了一半就跑」：**上游已经宣告过 `finish_reason`（内容已完整），只差最后那个终止字节 `data: [DONE]`**，而网关的判据是「没见过 `[DONE]` 且吐过内容 ⇒ 判截断」⇒ 于是它把**一份已经写完的回答**判成本轮失败，在流尾补一帧 error 再补 `[DONE]`。客户端读到 error 帧就显示「本轮运行失败」，你看到的就是「输出完就中断」。
>
> 本版把判据放宽一档：**上游已宣告过非空 `finish_reason` ⇒ 按正常收尾**（只补 `[DONE]`，不补 error 帧、不喂熔断、不罚账号）。真截断（吐了半句、从没宣告完成）**语义不变，仍然报错**。
>
> 顺带回答你另一个问题：**`/v1/models` 返回的是「这批账号的并集」，不等于「每个账号都有这个模型」** —— 详见第一节末尾。

---

## 一、本版做了什么

| # | 事项 | 来源 |
| --- | --- | --- |
| 1 | 修「回答已完整（`finish_reason` 已到）、只差 `[DONE]` 的尾部断流被误判为本轮失败」 | 你的报障（回退版本的原因） |
| 2 | 同一条放宽同步到 Anthropic `/v1/messages` 与 Responses `/v1/responses` 两条协议 | 同上（三个协议共用一套判据） |
| 3 | 解释「`/v1/models` 有返回 ≠ 每个账号都能发这个模型」 | 你的提问 |

### 1.1 你报的错（原文）

```
对话过程中对话会输出完就中断，这是我为什么回退版本的原因。
```

（你同时问：「这几个不是都有接口返回吗？为什么还会这样子？」—— 见 1.3。）

### 1.2 现场取证：这不是你的错觉，也不是偶发

我从你机器上三份日志里把这件事对出来了：

| 证据源 | 内容 |
| --- | --- |
| 网关日志 `gateway_data\gateway.log` | **16 条** `chat uid=… product=workbuddy model=deepseek-v4.1-flash: 上游流读取失败：…（已收 N 个有效帧 / M 字节，saw_done=false）`，**全部 `saw_done=false`**；两种底层错误：`peer closed connection without sending TLS close_notify`（多数）与 `cannot decrypt peer's message`（少数） |
| 同日志，16 次命中的账号 | **全部是 `www.workbuddy.ai` 国际版账号**（8ea1fcbc / 5ffd497c / 469348ed / 7ee497cc / 3b33a9a8 / 3af19916 六个），已收字节从 29,628 到 **1,992,314** 不等 |
| 客户端插件日志 | **18 次** `[acp-proxy: upstream stream truncated (openai) — emitting in-band error]` |
| 失败率 | 同期 `deepseek-v4.1-flash` 共 13,780 次记录 ⇒ **16 / 13,780 ≈ 0.12%** ⇒ 偶发，不是账号被封（6 个账号在 `state.json` 里全部 `disabled=false`、无冷却，`success_count` 1,004–2,751，`last_success` 都在当天） |

★ 关键一条：日志里网关自己那条「上游未发 `[DONE]` 就结束」（干净 EOF 分支）**0 条** —— 说明断的都是**传输层**（连接被掐），不是上游「吐完了却不写 `[DONE]`」。而掐断的时刻集中在**请求发出后 32 ms – 6 s**（最大一次 prompt 32.5 MiB / 5,307 条消息），并且**多个会话常在几十到几百毫秒内同时断** ⇒ 是共享通道（代理/链路）的短时抖动，与某个账号、某个模型无关。

### 1.3 回答你另一个问题：`/v1/models` 是并集，不是「每个账号都有」

`/v1/models` 返回的是**所有账号能力的并集**（国服 + 国际版合并去重，实测 185–189 条）。同一时刻：

- 国服账号有 `space-bunny`、`hy3-x`、`glm-5.1`、`deepseek-v4-pro` 等 **9 个国际版没有**的模型；
- 国际版账号有 `gpt-6-sol`、`gpt-6-astra`、`gemini-3.5-flash` 等 **16 个国服没有**的模型。

所以「列表里有」只代表**池子里有账号能发它**，不代表**随便抽到的那个账号能发它**。这就是 1.0.32 / 1.0.33 两版在修的东西：1.0.32 让裸名能选到号（修 503），1.0.33 让裸名不要选到**错的平台**（修 400 `model_not_in_upstream`），并且**启动即预热**上游真值，让裸名不要选到**错的区域**（修 400 `model_not_in_region`）。

---

## 二、根因：把「内容已完整」误判成「截断」

### 2.1 上游收尾的固定顺序

```
… data: {"choices":[{"delta":{"content":"…"}}]}        ← 正文（可能几千帧）
    data: {"choices":[{"delta":{},"finish_reason":"stop"}]}   ← ① 宣告完成
    data: {"usage":{…}}                                     ← ② 用量（可选）
    data: [DONE]                                             ← ③ 终止字节
```

**① 与 ③ 之间那一瞬，正是连接最容易被掐断的位置**（模型已经算完、连接进入短暂空闲）。掐在这里，客户端拿到的是**一份完整回答 + 没有终止字节**。

### 2.2 旧判据（1.0.33 及以前）

网关的帧迭代器只记两个标志：`saw_done`（见过 `[DONE]`）与 `saw_any_frame`（吐过 data 帧）。判据是：

```
if saw_done || !saw_any_frame { 正常收尾 } else { 判截断，补 error 帧 }
```

⇒ 「内容吐完、`finish_reason` 已到、只差 `[DONE]`」落进 else 分支 ⇒ **判成本轮失败**。

源码里当时还专门写了理由（`upstream/sse.rs`）：「不能用 `finish_reason` 代替 `[DONE]`，因为上游是先给 `finish_reason` 再写 `[DONE]`，那一瞬恰是空闲超时最容易掐断处。」—— **这句话的前半段是对的，后半段的推论反了**：正因为那一瞬最容易断，才更不能把「已经写完的回答」判成失败。

### 2.3 本版判据

新增第三个标志 `saw_finish_reason`（只在归一化之后判 **非空字符串** 的 `choices[].finish_reason`，`null` 不算）：

```
if saw_done || !saw_any_frame || saw_finish_reason { 正常收尾 } else { 判截断，补 error 帧 }
```

| 场景 | 旧行为 | 本版行为 |
| --- | --- | --- |
| 完整流（含 `[DONE]`） | 正常 | 正常（不变） |
| 一帧都没读到（空流） | 正常（合法空回合） | 正常（不变） |
| **内容完整 + `finish_reason` 已到 + 只差 `[DONE]`** | **判失败** | **正常收尾**（只补 `[DONE]`） |
| 吐了半句就断（从没宣告完成） | 判失败 | 判失败（**不变**） |

★ 为什么 `null` 不算：网关的归一化层会给**每个**缺键的帧补上 `"finish_reason": null` ⇒ 「键存在」不等于「上游宣告完成」，必须判**非空字符串**。

### 2.4 改动清单（4 个文件）

| 文件 | 改动 |
| --- | --- |
| `crates/ai-gateway-router/src/upstream/sse.rs` | 新字段 `saw_finish_reason` + 访问器 + 自由函数 `payload_has_finish_reason`；`premature_end()` 判据加一项；**两条**归一化分支（正文帧 / EOF 残余缓冲）都置位 |
| `crates/ai-gateway-router/src/protocol/anthropic_stream.rs` | `failure_message(saw_done, saw_any_frame, saw_finish_reason)` 加第三参 + 判据加一项 |
| `crates/ai-gateway-router/src/protocol/responses_stream.rs` | 同上 |
| `crates/ai-gateway-router/src/server.rs` | 新常量 `TAIL_DONE_FRAME`；两处协议收尾调用传三参；`chat_stream_response` 的读错误分支新增一条：**`saw_finish_reason` 为真 ⇒ 补 `[DONE]` 正常收尾**（日志：「上游已发 finish_reason、仅终止字节 [DONE] 未到（读错误：…），按正常收尾补发 [DONE]」） |

★ 刻意**不放宽**的一条：**首帧之前**就断（一个 token 都没交付）走的仍是既有路径（503 + 换账号重试）。那种情况重试才是对的。

---

## 三、实测证据（全部隔离实例，**你的服务全程未动**）

### 3.1 假上游复现台：5 种断流形态 A/B 对照

本机起了一个可控假上游（能按指令在任意时刻 RST），对同一份假上游分别跑 **1.0.33 出厂网关**与**本版网关**：

| 模式 | 假上游行为 | 1.0.33 | **1.0.34** |
| --- | --- | --- | --- |
| A | 2 帧 + `finish_reason` → 150 ms 后 RST | 补 error 帧 ❌ | **无 error 帧** ✅ |
| B | 半句就 RST（无 `finish_reason`） | 报截断 | 报截断（**保持**） ✅ |
| C | 吐完 + `finish_reason` + 干净 `end()`（无 `[DONE]`） | 补 error 帧 ❌ | **无 error 帧** ✅ |
| D | 只吐一帧就 `end()` | 报截断 | 报截断（**保持**） ✅ |
| E | 完整（含 `[DONE]`） | 正常 | 正常 ✅ |

五种形态的 `data: [DONE]` 计数**恒为 1**（不会出现双终止字节）。

### 3.2 三条协议都验过

| 端点 | 完整流（模式 C） | 真截断（模式 B） |
| --- | --- | --- |
| `/v1/messages`（Anthropic） | 200 / `event: message_stop` / 无 error 事件 | 200 / 有 error 事件 / 无 stop |
| `/v1/responses`（Responses） | 200 / `response.completed` | 200 / `response.failed` |

### 3.3 真机隔离实例（27 个你的真实账号的只读副本）

配置复刻你机器上的形状（`pool.product_models` 只声明 qoder + zcode、trae 目录为空、走同一个代理）：

- 裸名 `space-bunny` **6/6 全 200**（1.4 s – 7.5 s）；
- `deepseek-v4.1-flash` **9/9 全 200**（含 3 次同 `conversation_id` 的粘性会话）；
- **合计 15/15 无失败**，每一条都 `hasDone=true`、无截断码。

启动横幅（出厂二进制实测）：

```
product_models 缺 workbuddy：已补 30 个模型作为**兜底**（只用于「不排除」，不参与「声明」竞争）
product_models 缺 trae：已补 58 个模型作为**兜底**（只用于「不排除」，不参与「声明」竞争）
product_models 缺 trae：兜底清单标记为**权威**（未命中 = 确定没有，其账号不参与裸名兜底抽签）
[warmup] 上游模型真值预热完成（观测集已填充，裸名请求不再选中错区账号）
rust gateway listening on http://127.0.0.1:17905 (accounts=27)
```

### 3.4 约束遵守

实测全程你的 `:7864` / `:3080` / `:7890` 始终在监听；隔离实例一律**按 PID** 启停（且校验可执行文件路径），**从未按进程名杀进程**，也**从未运行安装器**。

---

## 四、验证与测试基线

### 4.1 测试

| 项 | 结果 |
| --- | --- |
| `cargo test --workspace` | **3045 passed / 0 failed / 6 ignored**（1.0.33 基线 3041） |
| `cargo test -p ai-gateway-router --lib` | **1685 passed / 0 failed / 1 ignored**（1.0.33 基线 1681） |

本版新增 **4 个单测**：

- `upstream/sse.rs`：`finish_reason_without_done_is_not_flagged_as_premature`（**你报障的本体**：`finish_reason` 已到、无 `[DONE]` ⇒ 必须**不**判提前结束）、`null_finish_reason_does_not_count_as_completion`（`null` 不算宣告完成）、`finish_reason_in_tail_buffer_counts_too`（最后一段**不带结尾换行**、只在 EOF 冲缓冲时被归一化 ⇒ 也要算）；
- `protocol/anthropic_stream.rs`：`finish_reason_without_done_is_not_a_failure`（同时钉死「上游显式 error 帧仍优先」）。

★ 既有的 `truncated_stream_is_flagged_but_content_is_preserved` **未改动、仍通过** —— 它的输入没有 `finish_reason`，所以新分支不生效，证明这次是**加法**而不是改写既有语义。

### 4.2 构建链

- 内嵌网关 `crates/ai-gateway-core/embedded/gateway.exe` = **13,831,680 B / SHA256 `9447272AACA62EA486C470E68CD18D745C1DD89A3CACE5932AD9CF65D289F2F8`**（由 `scripts/build-gateway.ps1 -Force` 从源码重建；= `target\release\gateway.exe`，逐字节相同）；
- 二进制字节探针（release 会剥符号，**唯一可靠判据**）：命中 `上游已发 finish_reason` / `仅终止字节 [DONE] 未到` / `兜底清单标记为**权威**` / `[warmup] 上游模型真值预热完成` = true。

---

## 五、客户端安装包

```
dist\AI Gateway_1.0.34_x64-setup.exe   14,626,546 B
SHA256 6B87A39906CF40743DFF515331398B88EBE3E9C5E0EF180B0886A4E85550E476
  .sig  424 B
SHA256 66A7A984C1E0E6DF5D0A312D9FF329C9703E3FD7C3A7087799DB2EB52722FE9E
```

签名与验签（密码学实测）：用 `tauri.conf.json` 的 `plugins.updater.pubkey` 解出公钥，Node `crypto.verify` 验本版安装包 —— **文件签名 VALID**、**trusted comment 全局签名 VALID**。

★ 提醒：NSIS **同版本不覆盖**（`installer.nsi:233-259` 用 `nsis_tauri_utils::SemverCompare` 比 `DisplayVersion`，相等走维护页）⇒ 每次交付都必须升版本号 —— 本版已升到 1.0.34。

---

## 六、本轮**未验证**的面（如实说明）

- 安装包**未在你的实机上安装**（你不在电脑旁，且约束禁止在你的实例存活时运行安装器）。本版所有真机证据都来自**同一份网关二进制**的隔离实例。
- 你实机现在跑的是 **v1.0.6（Go 网关）**（`D:\APP\AI Gateway\ai-gateway.exe` = 32,838,656 B / `FileVersion 1.0.6`，2026-10-07 07:40 换的）—— 本版的修复**不在**你现在跑的这份里，需要装 1.0.34。
- **断流本身不是网关能消灭的**：本版做的是「断流已经发生后，不再把完整的回答判成失败」。掐断你连接的那一层（代理 / 链路抖动）仍在 —— 只是从「你的回答被标成本轮失败」变成「回答照常收尾」。
- 老样子，以下仍未验证：Trae 真 JWT 路径、Qoder OAuth `ready`、DST 异常时区月步进、Trae 新加坡/国际版。
- 隔离实例的额度刷新被上游拒（`credit-refresh … http 403 code=10085`），**不影响对话**（本机账号副本被上游限流属已知现象）。

---

## 七、自动更新通道（状态未变）

`iuuuuuuuu/ai-gateway` 仍是 **private**，而 `tauri-plugin-updater` 是**匿名 HTTP 拉取、不带任何凭据** ⇒ 私有仓库对匿名请求一律 **404**，桌面端自动更新**仍然失效**。要恢复只有两条路：**改回 public**，或**把 `endpoints` 换成别的托管点**（自建静态站 / 对象存储 / 另一个公开仓库）。
