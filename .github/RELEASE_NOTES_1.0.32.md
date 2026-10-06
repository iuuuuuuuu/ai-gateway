## AI Gateway 1.0.32 正式版

> 本版修你报的那一个错：**裸模型名 `space-bunny` 调用返回 503 `no_healthy_account`（「所有账号都不可用」）**。根因不在账号 —— 账号是好的，是网关把「**内置静态兜底清单里没有它**」错当成了「**确定没有任何账号能服务它**」：你机器上的 `pool.product_models` 只声明了 qoder 与 zcode，workbuddy 只能靠那张 2026-09-18 的快照表，而 `space-bunny` 是那之后才上线的。本版把「猜测」与「事实」分开：**上游真报上来的模型清单**现在会被记住并用于放宽判断，但**永不参与「声明」**（否则会重演 2026-09-21 的 qoder 抢号事故）。顺带修掉同一条路上暴露的第二个缺陷：裸名选到「没有这个模型的区域」的账号 ⇒ 上游 400。

---

## 一、本版做了什么

| # | 事项 | 来源 |
| --- | --- | --- |
| 1 | 修「裸名 `space-bunny` ⇒ 503 `no_healthy_account`」 | 你的报障（截图） |
| 2 | 修「裸名选到没有该模型的区域账号 ⇒ 400 `model_not_in_region`」 | 同轮真机实测暴露 |
| 3 | 回答「这个模型的最大输出是多少」 | 你的提问 |

你的报障原文：

```
模型请求重试已取消（4/5）
失败原因：503: {"code":"no_healthy_account","message":"all accounts unavailable (cooling/disabled)","type":"api_error"}
```

以及你的那句话：「我使用这个模型居然报错没有健康的账号，这个模型明明 workbuddy 这个平台有的」—— **你说得对，它确实有，是我们的判据错了。**

---

## 二、根因：把「兜底清单没有」当成了「确定没有」

网关为每个平台维护三张模型清单，来源与可信度完全不同：

| 清单 | 来源 | 可信度 | 作用 |
| --- | --- | --- | --- |
| `product_model_set` | 宿主 GUI 下发（`gateway_native_config.json` 的 `pool.product_models`） | **可信**（人工维护的声明） | 参与「不排除」**和**「声明」 |
| `product_model_fallback_set` | 网关内置静态表（当前是 2026-09-18 的快照，30 条） | **猜测** | 只参与「不排除」 |
| `product_model_observed_set`（**本版新增**） | 上游 `/v1/models` 真报上来的 id | **实测事实** | 只参与「不排除」 |

你机器上 `pool.product_models` 只声明了 `qoder`（18 条）与 `zcode`（1 条），**没有 `workbuddy` 这一项** ⇒ workbuddy 只能靠第二张（快照）表，而 `space-bunny` **不在里面**。

旧判据是「兜底清单未命中 ⇒ 这个平台不服务它 ⇒ 排除」⇒ **27 个 WorkBuddy 账号全被排除** ⇒ 选号返回 `None` ⇒ 网关回落到那条最通用的 503 文案 `all accounts unavailable (cooling/disabled)`。

★ 这条文案本身也在误导：它说「冷却/停用」，真实原因却是**根本没有账号被纳入候选**。文案是客户端的硬契约，本版没动它，但根因已经不在了。

**修复（三层，逐层收窄风险）**

1. **记住上游真值**（`pool.rs:1106 observe_product_models`）：`/v1/models` / `/v1/models/regions` 每次被调用，网关就把上游真报上来的模型 id 记进池子（国服 / 国际版**分别**记）。它**只放宽「不排除」，永不参与「声明」** —— 这条边界是刻意的：把猜测塞进声明集会让别的平台的模型被抢到 workbuddy 账号上，正是 2026-09-21 qoder 事故的形态。
2. **区分「猜测」与「事实」**（`pool.rs:1226 product_definitely_lacks_model`）：只有**宿主声明清单**里明确没有，才算「确定没有」；「静态兜底清单没跟上」只降级为第三档 `guess_excluded`（先别急着用），在**没有任何平台声明**这个模型时仍然给它机会。
3. **严格分层**（`pool.rs:2575 pick_for_model_any`）：
   - ① 有平台**声明**过 ⇒ 候选 = 确定没有的 + 未知的，挑不到就**直接返回 `None`**（不跨到未知平台）；
   - ② 没有任何平台声明 ⇒ 候选 = 确定没有的 + `guess_excluded`；
   - ③ `guess_excluded` 为空 ⇒ 返回 `None`。

⇒ `space-bunny` 现在走 ②：上游实测集把它从「排除」里救回来 ⇒ 27 个账号重新进入候选 ⇒ 200。

**为什么不是「把 `space-bunny` 加进静态兜底清单」**：那是打地鼠 —— 上游每上一个新模型就要改一次代码、重发一次包，而且与「静态表是猜测」这个定位自相矛盾（下一张快照又会落后）。本版改成**由上游自己告诉我们**它有什么。

---

## 三、顺带修掉的第二个缺陷：裸名选到错区的账号

修好 ② 之后真机立刻暴露下一个问题：`space-bunny` **只在国服上游存在**，国际版没有。裸名（不带 `国服:` / `国际版:` 前缀）请求以前会在两区账号里随机挑，挑到国际版就吃上游 400 `model_not_in_region`。

本版按**上游真值**记录「哪个平台在哪个区见过哪些模型」（`pool.rs:1140 observe_product_models_in_region` / `:1192 model_region_hint`）：

- 只有当**国服与国际版都已经被拉取过**（否则「只在一个区见过」可能只是另一个区还没拉），且**恰好只有一个区见过**该模型时，才把候选收敛到那个区；
- 两个区都见过 ⇒ **不下结论**（否则会白丢一半额度）；
- 只在「区域不限」的请求上生效，**不覆盖你显式写的区域前缀**；
- 收紧后候选为空 ⇒ **退回原集合**（宁可不收敛，也不给「所有账号不可用」这种误导文案）。

⇒ 裸名 `space-bunny` 现在只会落在国服账号上；而 `国际版:space-bunny` 仍然是 400 `model_not_in_region` —— 这是**对的**：显式前缀尊重你的选择，上游真值确实没有它，网关不静默改区。

---

## 四、你问的「这个模型的最大输出是多少」

| 项 | 值 | 来源 |
| --- | --- | --- |
| 最大输出 | **128,000** tokens（`max_output_tokens`） | 网关 `/v1/models` |
| 上下文长度 | **1,000,000** tokens（`context_length`） | 同上 |
| 思考档位 | `low` / `medium` / `high` / `xhigh` / **`max`**（默认 `max`） | 同上 |
| 输入模态 | `text` + `image`（`supports.vision = true`） | 同上 |
| 区域 | **仅国服**（`supported_regions: ["cn"]`） | 同上 + 上游真值 |

---

## 五、实测证据

### 5.1 隔离实例（**全程不碰你正在用的 `:7864`**）

`wt-port\test-bin\inst-1032-live\`，跑的是**安装包里那份网关**（`inst-1032-unpack\embedded-gateway.exe`，13,778,944 B，SHA256 `CEA67DE0…`，与实例里那个 exe **逐字节一致**），监听 `:17866`，27 个 WorkBuddy 账号，`multi_product=true`，配置里 `pool.product_models` 只有 qoder + zcode ⇒ **正是你机器的前置条件**。

| 探针 | 结果 |
| --- | --- |
| **裸名 `space-bunny` × 20 连打** | **OK=20 / FAIL=0**（对照：你机器上是 503） |
| `国服:space-bunny` | 200（1,577 ms） |
| `workbuddy:国服:space-bunny` | 200（10,333 ms） |
| sticky 裸名 `space-bunny`（同一 `conversation_id` 连打 2 次） | 2/2 200 |
| 裸名 `gpt-6-sol`（**国际版独有**模型，反向对照） | 200 |
| sticky `gpt-6-sol` | 200 |
| 裸名 `hy3-x` / `kimi-k2.7` / `deepseek-v4-pro` / `default-model` | 全 200 |
| 回归：`glm-5.3` / `deepseek-v4.1-flash` | 全 200 |
| `国际版:space-bunny` | 400 `model_not_in_region`（**预期**，见第三节） |

`/debug/capability`（`:17866`）：`cn has space-bunny = True` / `intl has space-bunny = False` / `cn count = 17` / `intl count = 24`。

**约束④（不占用、不杀死你的服务）全程成立**：实测期间你的 `:7864` / `:3080` / `:58354` / `:7890` 始终在监听。

### 5.2 测试基线

| 项 | 结果 |
| --- | --- |
| `cargo test --workspace` | **3038 passed / 0 failed / 6 ignored**（EXIT=0；1.0.31 基线 3029） |
| `cargo test -p ai-gateway-router --lib` | **1678 passed / 0 failed / 1 ignored**（1.0.31 基线 1669） |
| `cargo test -p ai-gateway-core --lib` | 1285 passed / 0 failed / 4 ignored（不变） |
| `cargo check -p ai-gateway-router --lib` / `--bin gateway` | EXIT 0，0 warnings |

本版新增 **9 个单测**，全部在 `crates/ai-gateway-router/src/pool.rs`，把上面三条判据（含「Trae 保护」与「区域只有一边见过就不下结论」两条防回归）锁死：

- `bare_model_missing_from_static_fallback_still_reaches_healthy_accounts`（:4599）—— 你的报障本体；
- `observed_live_models_relax_exclusion_but_never_declare`（:4646）—— 实测集只放宽、不声明；
- `observe_product_models_needs_an_existing_fallback_list`（:4674）；
- `product_definitely_lacks_model_keeps_host_list_authoritative`（:4701）—— 只有宿主清单能说「确定没有」；
- `bare_region_pick_never_crosses_to_a_product_that_lacks_the_model`（:4732）；
- `bare_model_only_in_cn_never_picks_an_intl_account`（:4795）—— 区域收敛；
- `region_hint_requires_both_regions_fetched`（:4850）—— 只拉了一个区就不下结论；
- `region_narrowing_falls_back_when_target_region_is_unavailable`（:4881）—— 收紧后为空要退回；
- `sticky_pick_rejects_an_account_in_the_wrong_region`（:4902）。

### 5.3 安装包解包核验（**不运行安装器**）

- `FileVersion = 1.0.32` / `ProductVersion = 1.0.32` / `ProductName = AI Gateway`；
- 包内 GUI 的新命令符号 `read_gateway_log` / `reveal_gateway_log` / `logBytes` 均命中；
- 包内 16 个文件，含 `embedded-gateway.exe`（13,778,944 B）与 `_up_\assets\{zcode-captcha, wb-device-token}`。

★ 一条**写文档时踩到的坑**（留给后来者）：对包内 `embedded-gateway.exe` 搜 Rust 函数名（`product_definitely_lacks_model` 等）**全部 0 命中** —— release 构建会剥掉符号，**不能**用字符串命中来证明「新代码进包」。本版「新代码确实在包里」的判据是 ①包内 GUI 的 Tauri 命令名（能命中）+ ②**行为实测**（5.1 节，20/20）。

### 5.4 签名验签（密码学实测，VALID）

拿 `tauri.conf.json` 的 `plugins.updater.pubkey` 解出公钥，用 Node `crypto.verify` 验本版安装包：**文件签名 VALID = true**，**trusted comment 全局签名 VALID = true**。

细节：minisign 的 alg 字段是 `ED`（**预哈希** BLAKE2b-512），必须 `verify(null, blake2b512(file), key, sig)` 才对；直接对文件原始字节验签会得 false，**这是正常的**。另：公钥与 `.sig` 都是**外面包了一层 base64 的文本**，要先解码才看得到 minisign 四行结构。

---

## 六、本轮**未验证**的面（如实说明）

- 安装包**未在你的实机上安装**（你不在电脑旁，且约束禁止在你的实例存活时运行安装器）。本版所有真机证据都来自**安装包里那份网关**的隔离实例。
- 「国际版 `space-bunny` ⇒ 400」是**上游真值**（`intl has space-bunny = False`）。如果腾讯以后在国际版上线它，网关会**自动**跟着放开（实测集每次拉 `/v1/models` 都会刷新）。
- 隔离实例的额度刷新被上游拒（`credit-refresh … http 403 code=10085`，共 33 行），**不影响对话**（本机 27 个账号是同一批账号的副本，刷新被限流属已知现象）。
- 老样子，以下仍未验证：Trae 真 JWT 路径、Qoder OAuth `ready`、DST 异常时区月步进、Trae 新加坡/国际版。

---

## 七、客户端安装包与纯部署产物

```
dist\AI Gateway_1.0.32_x64-setup.exe   14,598,416 B
SHA256 BD07D32CC0CA69845CF0504C5905C03BCD1832FAA9F3109F606676EBFEA55A38
  .sig  424 B  keyid 6FA3C36F4BE67C7E（原始字节序 7e7ce64b6fc3a36f —— 同一把钥匙的两种写法）

非客户端纯部署（单文件，无 GUI / 无 WebView2 / 无安装器）：
dist\ai-gateway_1.0.32_server-x64.exe   27,063,808 B
SHA256 1788241A21F5803BD8345F690EAE477EEDE37B1AE174B3F32A97A7F1021110A2
  启动：ai-gateway_1.0.32_server-x64.exe serve --port 57890
```

你实机是 **1.0.21**，直接装 1.0.32 即可（1.0.22–1.0.32 的修复全在里面）。

★ 提醒：NSIS **同版本不覆盖**（`installer.nsi:233-259` 用 `nsis_tauri_utils::SemverCompare` 比 `DisplayVersion`，相等走维护页）⇒ 每次交付都必须升版本号 —— 本版已升到 1.0.32。

★ 现成的绕法（不用等安装包）：把模型名写成 **`国服:space-bunny`**，它走的是显式区域前缀，从来不受本缺陷影响。

---

## 八、自动更新通道（状态未变）

`iuuuuuuuu/ai-gateway` 仍是 **private**，而 `tauri-plugin-updater` 是**匿名 HTTP 拉取、不带任何凭据** ⇒ 私有仓库对匿名请求一律 **404**，桌面端自动更新**仍然失效**。这与上游仓库 404 无关，是我们自己的可见性设置。要恢复只有两条路：**改回 public**，或**把 `endpoints` 换成别的托管点**（自建静态站 / 对象存储 / 另一个公开仓库）。
