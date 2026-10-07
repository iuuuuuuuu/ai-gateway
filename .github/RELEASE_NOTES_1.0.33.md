## AI Gateway 1.0.33 正式版

> 本版修你报的那一个错：**裸模型名 `space-bunny` 调用返回 400 `model_not_in_upstream`（「两条产品线的 function 池（chat_v3 / solo_work_lite）都拒绝了 space-bunny」）**。
>
> 1.0.32 修好了「503 选不出号」，但**同时把 Trae 账号放进了候选池**：你机器上的 `pool.product_models` 只声明了 qoder 与 zcode，workbuddy 与 trae 都只能靠网关内置的兜底清单，而 `space-bunny` 两张表里都没有 ⇒ 27 个 WorkBuddy 账号与 **1 个 Trae 账号**一起掉进最后一档「先别急着排除」⇒ 约 3% 的概率抽到 Trae ⇒ 上游回 4001 ⇒ 你看到那条 400。本版把 Trae 那份兜底清单标记为**权威**（它派生自客户端别名表，是 Trae 唯一能解析的名字集合）⇒ 它的账号被硬排除，最后一档只剩 WorkBuddy。
>
> 顺带修掉真机实测暴露的第二个缺陷：**启动窗口内观测集为空**，裸名请求会选中「没有这个模型的区域」的账号（你 07:3x 的两次现场都落在窗口里）。现在网关**启动即预热**上游真值。

---

## 一、本版做了什么

| # | 事项 | 来源 |
| --- | --- | --- |
| 1 | 修「裸名 `space-bunny` ⇒ 400 `model_not_in_upstream`（抽到 Trae 账号）」 | 你的报障（截图） |
| 2 | 修「启动窗口内裸名选中错区账号 ⇒ 400 `model_not_in_region`」 | 同轮真机实测暴露 |

你的报障原文：

```
本轮运行失败 400: {"code":"model_not_in_upstream","message":"两条产品线的 function 池（chat_v3 / solo_work_lite）都拒绝了「space-bunny」：上游认为这个模型名在这个账号上不存在。这不是白名单问题 —— 把它加进白名单也不会生效，请改用上游真实存在的模型。","type":"api_error"} INVALID_REQUEST
```

以及你的那句话：「我是用这个模型还是提示这个」—— **模型没问题，是网关把它发给了错误的平台。**

★ 值得先说明的是：**这条错误码本身就证明 1.0.32 的修复生效了**。503 `no_healthy_account` 的意思是「一个账号都没选出来」，而这次的 400 说明**号选出来了、请求也打到上游了**，暴露的是下一层的问题（选到了哪个平台）。

---

## 二、根因：Trae 的兜底清单被当成了「猜测」，于是它的账号进了抽签池

网关为每个平台维护三张模型清单：

| 清单 | 来源 | 可信度 | 作用 |
| --- | --- | --- | --- |
| `product_model_set` | 宿主 GUI 下发（`gateway_native_config.json` 的 `pool.product_models`） | **可信**（人工维护的声明） | 参与「不排除」**和**「声明」 |
| `product_model_fallback_set` | 网关内置静态表 | 视产品而定（**本版新增区分**） | 只参与「不排除」 |
| `product_model_observed_set` | 上游 `/v1/models` 真报上来的 id | **实测事实** | 只参与「不排除」 |

你机器上 `pool.product_models` 只声明了 `qoder`（18 条）与 `zcode`（1 条），**没有 `workbuddy`、也没有 `trae`** ⇒ 两者都只能靠内置兜底清单。裸名 `space-bunny` 的逐账号分流是：

| 平台 | 账号数 | 兜底清单里有 `space-bunny` 吗 | 落进哪一档 |
| --- | --- | --- | --- |
| qoder | 2 | —（宿主清单已声明，且不含它） | `scoped`（确定没有） |
| zcode | 2 | —（同上） | `scoped`（确定没有） |
| workbuddy | 27 | 否（2026-09-18 快照，比 `space-bunny` 上线早） | `guess_excluded`（先别急着排除） |
| **trae** | **1** | **否（别名表里没有）** | **`guess_excluded`（同上）** |

`declared` 为空、`unknown` 为空 ⇒ 走到最后一档「兜底」：候选 = 27 个 WorkBuddy **+ 1 个 Trae** ⇒ **约 1/32 ≈ 3% 抽到 Trae** ⇒ Trae 上游回 4001（`the param is invalid`）⇒ 网关如实报 400 `model_not_in_upstream`。

**修复：区分「兜底清单是否权威」。**

- **Trae 的兜底清单是权威的**：它派生自 `products/trae/dispatch.rs` 的 `ALIASES` 表 —— 那是**客户端唯一能发的名字集合**，也正是上游 `config_name` 解析的唯一真相。所以「别名表里没有」⇒「Trae 上游解析不了这个名字」是**确定结论**，该产品的账号必须被硬排除。
- **WorkBuddy 的静态表不是权威的**：它是 2026-09-18 的快照，只是「我们知道它当时有什么」。把它当权威会**立刻退回 1.0.32 修掉的那个 503**。

实现（三处，逐层收窄风险）：

1. 新字段 `product_fallback_authoritative: HashSet<String>`（`pool.rs:872`）+ setter `set_product_fallback_authoritative`（`pool.rs:1125`）+ 私有判据 `fallback_authoritative`（`pool.rs:1134`，要求「已标权威」**且**「兜底清单非空」）。
2. `product_definitely_lacks_model`（`pool.rs:1305`）：末行 `known` ⇒ `known || self.fallback_authoritative(product)` —— 「权威兜底清单未命中」成为第三种「确定没有」。★ 一处改动三处受益：它还同时管着裸名选号（`pool.rs:3022` 落点 G）与**粘性会话**（`pool.rs:2902` 落点 F）。
3. `pick_for_model_any` 分档（`pool.rs:2679`）：在「宿主清单说没有」与「兜底清单没跟上」之间**插入**一支 —— 该产品兜底权威且未命中 ⇒ 进 `scoped`（硬排除），不再进 `guess_excluded`。
4. 注入（`bin/gateway.rs`）：只有 `trae` 被标权威，并打印启动横幅 `product_models 缺 trae：兜底清单标记为**权威**…`。

⇒ 最后一档只剩 27 个 WorkBuddy 账号，Trae 那 3% 的抽签概率**归零**。

---

## 三、顺带修掉的第二个缺陷：启动窗口内观测集为空

修好 Trae 之后，A/B 真机实测（隔离实例 `:17867` 跑 1.0.32 / `:17868` 跑本版，各 27 个 WorkBuddy + 1 个 Trae 账号）暴露出**下一层**问题：

| 实例 | 冷启动（没人访问过 `/v1/models`） | 预热后 |
| --- | --- | --- |
| 1.0.32（`:17867`） | **0 / 120 成功** — 30× `model_not_in_upstream`（**正是你那条**）+ 90× `model_not_in_region` | 120 / 120 |
| 本版（`:17868`） | 0 / 120 — 120× `model_not_in_region`（**Trae 那 30 次已彻底消失**） | 120 / 120 |

两栏都说明：**观测集只在有人访问 `/v1/models` 时才被填充**（1.0.32 的设计）。宿主与客户端确实会定期打那个接口，但**进程刚起来的那段时间里它还是空的** —— 而你的两次现场（07:3x 的 `gpt-6-astra` 与 `space-bunny`）正好落在窗口内：区域真值没拉过 ⇒ `model_region_hint` 给不出结论 ⇒ 裸名可以选中**错区**的账号 ⇒ 上游回 `model [space-bunny] service info not found`。

**修复：启动即预热**（`server.rs:1684 pub async fn warm_up_live_models` + `bin/gateway.rs:923` 在 `axum::serve` 前 `tokio::spawn`）。

- 预热 = 一次 `region_snapshot`（每区一个上游请求）+ 喂观测集，**放在启动时是零延迟成本**（放进选号热路径则每个请求都要等它）；
- **失败静默**：预热是**优化**不是前提，拉不到就退化成既有行为（观测集为空 = 不额外排除），绝不因为预热失败而拒绝服务；
- 不阻塞监听（网络动作，最长可能几十秒），但也不晚于第一个请求。

⇒ 冷启动 120 轮 **120/120 全 200，0 失败**（见 5.1 实测）。

---

## 四、实测证据（全部隔离实例，**你的服务全程未动**）

### 4.1 决定性 A/B：1.0.32 vs 本版，同配置同账号

隔离实例目录 `test-bin\inst-1033-live\`：27 个 WorkBuddy 凭证 + 1 个 Trae 凭证（`trae-2593120869167228`）的只读副本；qoder / zcode 指向**空目录**（复现你「宿主清单只有 qoder + zcode」的形状）；`multi_product=true`、`api_key=sk-123`。

- `:17867` = 1.0.32 包内网关（13,778,944 B / `CEA67DE0…`）作对照组；
- `:17868` = 本版（13,835,264 B / `EAAD292967AF…`）。

启动横幅证实新代码生效（**对照组没有这一行**）：

```
product_models 缺 workbuddy：已补 30 个模型作为**兜底**（只用于「不排除」，不参与「声明」竞争）
product_models 缺 trae：兜底清单标记为**权威**（未命中 = 确定没有，其账号不参与裸名兜底抽签）
rust gateway listening on http://127.0.0.1:17868 (accounts=27)
[warmup] 上游模型真值预热完成（观测集已填充，裸名请求不再选中错区账号）
```

**冷启动（不访问 `/v1/models`，裸名 `space-bunny` × 120 轮）**

| 实例 | ok | fail | 失败构成 |
| --- | --- | --- | --- |
| `:17867` 1.0.32 | 0 | 120 | **`400 model_not_in_upstream` × 30**（你那条）+ `400 model_not_in_region` × 90 |
| `:17868` 本版（仅权威修复） | 0 | 120 | `400 model_not_in_region` × 120 —— **Trae 那 30 次彻底消失** |
| `:17868` 本版（权威 + 预热） | **120** | **0** | — |

★ 第一行那 30 次 `model_not_in_upstream` 是**你报障的逐字复现**（同一个 Trae 账号、同一个错误码），而它在本版里**归零**。

### 4.2 预热后的能力真值（`:17868`，启动后**无人访问** `/v1/models`）

```
cn_truth_known=True  intl_truth_known=True
dynamic_cn=17        dynamic_intl=25
cn_has_space-bunny=True   intl_has_space-bunny=False
```

⇒ 预热确实在启动时把上游真值灌进了池子，而不是等宿主来敲门。

### 4.3 约束遵守

实测全程你的 `:7864` / `:3080` / `:7890` 始终在监听；隔离实例一律**按 PID** 启停，**从未按进程名杀进程**，也**从未运行安装器**。

---

## 五、验证与测试基线

### 5.1 测试

| 项 | 结果 |
| --- | --- |
| `cargo test --workspace` | **3041 passed / 0 failed / 6 ignored**（1.0.32 基线 3038） |
| `cargo test -p ai-gateway-router --lib` | **1681 passed / 0 failed / 1 ignored**（1.0.32 基线 1678） |

本版新增 **3 个单测**，全部在 `crates/ai-gateway-router/src/pool.rs`：

- `authoritative_fallback_keeps_bare_model_off_that_product` —— **你的报障本体**：标 trae 权威后，裸名 `space-bunny` 连打 200 次**必须全落 WorkBuddy**；
- `authoritative_fallback_needs_a_non_empty_list` —— 只标权威、不给清单不算数（防止「标了就全排除」的过度收紧）；
- `sticky_pick_rejects_an_account_from_a_product_with_an_authoritative_list` —— 粘性会话同样受约束。

★ 既有的 `product_definitely_lacks_model_keeps_host_list_authoritative` **未改动、仍通过** —— 它没有标权威，所以新分支不生效，证明这次改动是**加法**而不是改写既有语义。

### 5.2 构建链

- 内嵌网关 `crates/ai-gateway-core/embedded/gateway.exe` = **13,835,776 B / SHA256 `1BEDC8E683BAE7EFEB7463A91D2FEB1A67734028EEEDF913F53CDEA05720AACC`**（由 `scripts/build-gateway.ps1 -Force` 从源码重建；= `target\release\gateway.exe`，逐字节相同）；
- 二进制字节探针（release 会剥符号，**唯一可靠判据**）：命中 `兜底清单标记为**权威**` / `[warmup] 上游模型真值预热完成` = true。

---

## 六、客户端安装包

```
dist\AI Gateway_1.0.33_x64-setup.exe   14,605,514 B
SHA256 20746313D2F709932AF270AB9B957A10B94982E97BB60B67EF31674A3850AAD2
  .sig  424 B
```

签名与验签（密码学实测）：用 `tauri.conf.json` 的 `plugins.updater.pubkey` 解出公钥，Node `crypto.verify` 验本版安装包 —— **文件签名 VALID**、**trusted comment 全局签名 VALID**。minisign 的 alg 是 `ED`（**预哈希** BLAKE2b-512），必须 `verify(null, blake2b512(file), key, sig)` 才对。

★ 提醒：NSIS **同版本不覆盖**（`installer.nsi:233-259` 用 `nsis_tauri_utils::SemverCompare` 比 `DisplayVersion`，相等走维护页）⇒ 每次交付都必须升版本号 —— 本版已升到 1.0.33。

---

## 七、本轮**未验证**的面（如实说明）

- 安装包**未在你的实机上安装**（你不在电脑旁，且约束禁止在你的实例存活时运行安装器）。本版所有真机证据都来自**同一份网关二进制**的隔离实例。
- 你实机现在跑的是 **v1.0.6（Go 网关）**（`D:\APP\AI Gateway\ai-gateway.exe` = 32,838,656 B / `FileVersion 1.0.6`，2026-10-07 07:40 换的）—— 本版的修复**不在**你现在跑的这份里，需要装 1.0.33。
- 老样子，以下仍未验证：Trae 真 JWT 路径、Qoder OAuth `ready`、DST 异常时区月步进、Trae 新加坡/国际版。
- 隔离实例的额度刷新被上游拒（`credit-refresh … http 403 code=10085`），**不影响对话**（本机 27 个账号是同一批账号的副本，刷新被限流属已知现象）。

---

## 八、自动更新通道（状态未变）

`iuuuuuuuu/ai-gateway` 仍是 **private**，而 `tauri-plugin-updater` 是**匿名 HTTP 拉取、不带任何凭据** ⇒ 私有仓库对匿名请求一律 **404**，桌面端自动更新**仍然失效**。要恢复只有两条路：**改回 public**，或**把 `endpoints` 换成别的托管点**（自建静态站 / 对象存储 / 另一个公开仓库）。
