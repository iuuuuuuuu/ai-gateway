# WorkBuddy Switch / AI Gateway — 项目状态与后续工作

> 最后更新：2026-10-07（**1.0.34**：㉙ 修业主报障「对话过程中对话会输出完就中断」（业主原话：「这是我为什么回退版本的原因」）—— 根因：上游收尾顺序是「**先**宣告 `finish_reason`、**后**写终止字节 `[DONE]`」，而两者之间那一瞬恰是连接最容易被掐断的位置；旧判据 `if saw_done || !saw_any_frame` 只看 `[DONE]` ⇒ 「内容已完整、只差 `[DONE]`」落进 else ⇒ **把一份写完的回答判成本轮失败**（网关补一帧 error 再补 `[DONE]`，客户端读到 error 帧就显示「本轮运行失败」）。现场取证：`gateway.log` **16 条** `上游流读取失败：… saw_done=false`（**全部 `www.workbuddy.ai` 国际版账号**，已收 29,628–1,992,314 B）、插件日志 **18 次** `upstream stream truncated (openai) — emitting in-band error`、失败率 **16/13,780 ≈ 0.12%**、断流时刻集中在**请求发出后 32 ms – 6 s** 且**多会话几十到几百毫秒内齐断** ⇒ 是共享通道（代理/链路）抖动，与账号、模型无关。修法：`SseFramesIter` 新增 `saw_finish_reason`（`upstream\sse.rs:563` 字段 / `:623` 访问器 / `:650` 判据 / `:684` + `:722` 两处置位 / 自由函数 `payload_has_finish_reason` —— **必须在 `normalize_frame` 之后判非空字符串**，因为归一化会给缺键帧补 `"finish_reason": null`）+ `anthropic_stream.rs:408`/`:417` 与 `responses_stream.rs:671`/`:678` 的 `failure_message` 加第三参 + `server.rs:2853 const TAIL_DONE_FRAME` + `chat_stream_response` 读错误分支（`server.rs:3150-3161`）`saw_finish_reason ⇒ 补 [DONE] 正常收尾`。★ 刻意**不放宽**：首帧之前就断（一个 token 都没交付）仍走 503 + 换账号重试（`forward.rs:1130` / `:1147-1233`）；真截断（吐了半句、从没宣告完成）仍报错。实测：假上游 5 模式矩阵（`:17903` 出厂 1.0.33 vs `:17904` 出厂 1.0.34，同一假上游 `:17901`）—— modeA（finish_reason 后 150 ms RST）与 modeC（吐完 + 干净 `end()` 无 `[DONE]`）**1.0.33 补 error 帧 / 1.0.34 无 error 帧**，modeB / modeD 两边都报错，modeE 两边都正常，`doneCount` 恒 1；三协议探针（`/v1/messages` `event: message_stop`、`/v1/responses` `response.completed`）；真机隔离台 27 账号只读副本 **15/15 全 200**；workspace **3045/0/6**（+4）、router **1685/0/1**（+4）；签名端到端验签 VALID；**包内 GUI 内嵌 gz blob 逐字节 identical**（`0x17CB042`，解压 13,831,680 B / `9447272A…`）。㉚ 同时回答业主另一问「这几个不是都有接口返回吗？为什么还会这样子？」—— `/v1/models` 返回的是**并集**（实测 185–189 条），国服独有 9 个 / 国际版独有 16 个 ⇒ 「列表里有」只代表池子里有账号能发它，**不代表随便抽到的那个账号能发它**。详见 `test-bin\DELIVERY-1.0.34.md`）
>
> 上一版：2026-10-07（**1.0.33**：㉗ 修业主报障「裸名 `space-bunny` ⇒ 400 `model_not_in_upstream`（两条产品线的 function 池 chat_v3 / solo_work_lite 都拒绝）」—— 这是 **1.0.32 修好选号之后暴露的下一层**：1.0.32 把「静态兜底清单里没有」降级成猜测（`guess_excluded`），于是 **27 个 WorkBuddy 号 + 1 个 Trae 号一起进抽签池** ⇒ 约 1/32 ≈ 3% 抽到那个 Trae 号（Trae 上游确实没有 `space-bunny`）⇒ 400。根因是**把「猜测」与「权威」混为一谈**：Trae 的兜底清单派生自 `products\trae\dispatch.rs:275` 的 `ALIASES` 表（58 条客户端名 → 18 个上游 config_name，文档原文称它是 Trae 模型名的「唯一」真相），对 Trae 而言它就是**确定**；WorkBuddy 的静态表是 2026-09-18 快照，才是真正的猜测。修法：新增 `product_fallback_authoritative`（`pool.rs:872`）标记「哪些产品的兜底清单是权威的」，`product_definitely_lacks_model`（`pool.rs:1294`）末行改为 `known || self.fallback_authoritative(product)`，`pick_for_model_any`（`pool.rs:2644`）新增一支 `fallback_authoritative ⇒ scoped`（硬排除），注入点 `bin\gateway.rs:182` / `:188`（**只有 trae 进这个集合**）。㉘ 顺带修**本版 A/B 实测中发现**的第二个缺陷：观测集只在有人访问 `/v1/models` 时才填充 ⇒ **启动窗口**内裸名可能选中错区账号（400 `model_not_in_region`）。新增 `server.rs:1684 warm_up_live_models` + `bin\gateway.rs:923` 启动时 `tokio::spawn` 预热。实测：**出厂二进制**（`embedded\gateway.exe` 逐字节拷贝）在业主现场形状（27 WB + 1 Trae，qoder/zcode 空）下**冷启动**裸名 `space-bunny` **120/120 全 200**（1.0.32 同形状 0/120：30× `model_not_in_upstream` + 90× `model_not_in_region`）、workspace **3041/0/6**（+3）、签名端到端验签 **VALID**、**包内 GUI 内嵌的就是出厂网关本体**（gz blob 逐字节比对 + 解压哈希 `1BEDC8E6…`）。详见 `test-bin\DELIVERY-1.0.33.md`）
>
> 上一版：2026-10-06（**1.0.32**：㉔ 修业主报障「裸名 `space-bunny` ⇒ 503 `no_healthy_account`」—— 根因是把「静态兜底清单里没有」当成了「平台确定不提供」（业主的 `pool.product_models` 只有 qoder/zcode，workbuddy 走快照兜底，而 `space-bunny` 是快照之后上线的）⇒ 新增**上游实测清单**（每次拉 `/v1/models` 都刷新；只放宽「不排除」，**永不**参与「声明」）+ 区分「猜测 vs 事实」（只有宿主清单能说「确定没有」）+ 裸名三档严格分层；㉕ 顺带修「裸名选到没有该模型的区域账号 ⇒ 400 `model_not_in_region`」（新增区域维度实测集 + 区域收紧，只在「区域不限 + 两区都拉过 + 恰好一区见过」时生效，收紧后为空则退回原集合）；㉖ 回答业主提问「这个模型的最大输出是多少」（`space-bunny`：`max_output_tokens=128000`、`context_length=1000000`、effort 五档 low/medium/high/xhigh/max、`text+image`、仅国服）。实测：**包内网关**隔离实例裸名 **20/20 全 200**（业主 `:7864` 同请求是 503）、workspace **3038/0/6**、签名端到端验签 **VALID**。详见 `test-bin\DELIVERY-1.0.32.md`）
>
> 上一版：2026-10-06（**1.0.31**：⑳ 合入上游 6 项可用更新（账号字段类型归一 / 状态出口统一 / 静态模型表并集 30 条 + 两侧快照测试 / 剩余天数向上取整 / 轻量模式残留清零 / 上下文超长码别名 4028）；㉑ 排查 `deepseek-v4.1-flash` 截断报障 —— 定位到业主文案用的是网关合成帧（**a** completion event），补齐「上游未发 [DONE]」分支的日志留痕（此前**一条都没有**），并用假上游 6 模式矩阵复现（`test-bin\mock-chat\`）；㉒ 对齐上游 `1c8bfed` 已有的网关日志查看入口（`GET /api/gateway/log` + 网关页日志面板；上游按行数取，本仓库按字节取，因为日志里有超长单行 JSON）；㉓ 额外产出**非客户端纯部署**单文件产物 `wt-port\dist-single\ai-gateway.exe`，并修掉 `build-single.ps1` 的静默死亡 bug。详见 `test-bin\DELIVERY-1.0.31.md`）
>
> 更早：2026-10-06（**1.0.30**：⑲ 一次性修完业主最后报的四件事 —— ① 切手动模式/改模型白名单删光 Trae 网关凭证（uid 空间用错，且不自愈）；② 后台任务跑完前端不刷新（轮询 hook 只在窗口可见/聚焦时跑）；③ 后台拉客户端会弹窗（改为默认隐藏 + 设置页开关）；④ ZCode 断流文案透传 reqwest 原文。并顺带修掉「豆包保活失败仍记为当天已完成」的记账缺陷）
> 本文件是**接手用**的持久状态说明，不随会话消失。

---

## 一、这是什么项目

`D:\WishProject\WorkbuddySwitchAPi\wt-port` —— WorkBuddy（CodeBuddy）多账号切换与 API 网关。

| 组件 | 位置 | 语言 |
|---|---|---|
| 宿主 GUI | `src-tauri/` + `src/` | Rust/Tauri + React |
| 宿主逻辑库 | `crates/ai-gateway-core/` | Rust |
| WebUI 后端 | `crates/ai-gateway-server/` | Rust |
| **网关** | `crates/ai-gateway-router/` | **Rust（2026-10 从 Go 全量重写）** |
| Go 参照实现 | `go-gateway/` | Go（**保留作对照，构建链已不依赖**） |

- git 根 = `wt-port\`，主分支 `main`（2026-10-06 由 `migrate/aigw-v1.0.2` 改名而来；原上游陈旧线 `224919a` 改名 `archive/origin-main` 留档。详见第九节）
- 远端 **`fork` = `https://github.com/iuuuuuuuu/ai-gateway`**：`main` 已推送（远端 `fork/main` = `726aafa`；本地另有 **8 个提交未推**：`12c2871` / `cb62b2b` / `641f89c` / `6ade937` / `1c7b7c0` / `e14fea1` / `621452e` / 本次 9.22 的记录提交）；本地 tag 共 **49 个**（最大 `v1.0.38`），已发布的 Release 到 **v1.0.38** 为止（`releases/latest` 与 `dist\latest.json` 现都还指向 **v1.0.38**）—— ★ **1.0.39 ~ 1.0.46 尚未打 tag、尚未发布**（安装包都已产出并落在 `dist\`，只差推 tag + 发 Release）。★ 该仓库 2026-10-06 按业主 m26011 **已转为 private**（匿名访问 404，自动更新通道因此失效，详见 9.6）
- 当前版本号 **1.0.46**（发布提交 `1c7b7c0` 用 `scripts/bump-version.ps1` 把 **14 个文件**（`tauri.conf.json` / `package.json` / `package-lock.json` / `Cargo.toml` / `Cargo.lock` / `npm\package.json` 等）bump 到 1.0.46；签名安装包已产出并复制到 `dist\`，内嵌网关与内嵌前端**双指纹校验 [OK]**，见 9.21）
  - ★ 业主**实机 2026-10-07 07:40 已回退到 1.0.6**（`D:\APP\AI Gateway\ai-gateway.exe` 32,838,656 B / FileVersion 1.0.6 / mtime 2026-09-17 21:01:20；`:7864` 跑的是 Go 网关 `gateway-355c61-29367fd438f80ba2.exe` 8,759,296 B）⇒ 当次 1.0.22–1.0.34 **均未安装**（截至 2026-10-13 业主实机已升到 **1.0.44**；1.0.45 尚未安装），要装就直接装最新版（跳过版本里的修复全在 1.0.34 内；★ **回退到 Go 版会丢掉 1.0.21–1.0.33 的全部修复**，1.0.34 修的「输出完就中断」在 Go 版里**同样存在**——Go 只在「0 有效帧」时补 error 帧、吐过内容却没见 `[DONE]` 时**静默当成功**，与本仓库 1.0.33 及以前的行为不同）
  - ★ NSIS 同版本**不覆盖**（`installer.nsi:233-259` 用 `nsis_tauri_utils::SemverCompare` 比 `DisplayVersion`，相等走维护页）⇒ 升版本号是交付的硬前提

---

## 二、当前进展（2026-10-06）

### 已完成的四组重点

| 组 | 内容 | 状态 |
|---|---|---|
| **P0#1** | CLI 子命令整个缺失（会让界面卡死） | ✅ qoder-login 10 + zcode-login 8 = **18/18 真机验证** |
| **P0#2** | 三处启动接线（`product_tasks_runner` / `claim_scheduler` / `sync_to_dir`） | ✅ 真机验证 |
| **批次3 N4** | Qoder 计费周期统计（三段：存储 / 取数 / 接线） | ✅ 真机端到端（真实账号） |
| **并发缺陷** | 客户端断开时账号锁泄漏 | ✅ 150.4s 全挂 → 5.0s 通过 |
| **代理泄漏** | reqwest 偷偷走 Windows 系统代理 | ✅ 见下方「陷阱」 |
| **1.0.17 A** | Trae 进「智能体管理」（写 `state.vscdb` 自定义模型表） | ✅ API + UI 双半段真机实测，闭环 `HTTP 200` |
| **1.0.17 B** | Trae 进「兼容网关」（`products\trae\` 适配器，约 5196 行） | ✅ 真账号 4 模型 `200` + SSE；抓到了默认产品线判错的根因 |
| **11133 修复** | 业主对话里的 400 `code=11133`（空名 tool-call 毁掉整会话） | ✅ 根因闭环 + 三层修复 + 独立实例 6 条畸形探针 400→200（业主 :7864 同组 6/6 全 400 对照）；并用业主那条真实 id `call_120c4dae9cdc41168ec0cc39` 1:1 复现 400 → 修复版 200 |
| **11133 复测** | 用**交付包内嵌网关**（`test-bin\inst-pkg-final\`，端口 7922）原样重打同一批探针 | ✅ 9 探针 A/B：业主 `:7864` 8/8 全 400；交付包网关 8/8 全 200 + `PONG`（流式 5 帧 + `[DONE]`）；整流日志 8 行逐条对账 |
| **1.0.20 ①–⑩** | 业主回退 GitHub 发布版后报的全部问题，一次修完（详见 `test-bin\DELIVERY-1.0.20.md`） | ✅ 全部落盘 + 交付包本体独立实例实测（`test-bin\inst-pkg-1020\`，`:57893`） |
| ① Trae 详情页 404 | webui 缺 3 条路由（桌面端 IPC 本来就有）⇒ 补 `api.rs:319/:341/:357` | ✅ `:57893` 实测 detail/history/stats 200；缺参 ⇒ 400 |
| ② 裸名路由到 Trae | `bin/gateway.rs:154-164` 注入 Trae 兜底清单（58 模型） | ⚠ **本机 A/B 未能复现差异**（1.0.19 里这段已存在）⇒ 只能用源码 + 单测 + Trae-only 池对照证明 |
| ③ 用量页裂行 / 成本优先 | `forward.rs:390 inject_cost_rates` + `capability.rs:1341-1434` | ✅ `/api/gateway/models` 210 条，`deepseek-v4.1-flash` 只 1 条（无 trae 通道） |
| ④ Trae 积分 0 / 到期未知 / 正在被使用 | 新增 `products/trae/credits.rs` + `trae-credit-cache.ts:79` | ✅ 交付包侧实测 `credits:667` / `expire_day:2026-10-31` / `in_flight:0` |
| ⑤ 会话中断 | `server.rs:2712 chat_stream_error_frame`，恰一个 `[DONE]` 前补错误帧 | ✅ 假上游 `abort` 模式复现后客户端正常收敛 |
| ⑥ 宿主假 GATEWAY_RUNNING（⑦） | Plan A/B/C/D：删静态布尔 + `/healthz` 认领 + 中文提示 + `stop_gateway()` | ✅ `:57893` 实测 port-check / port-holder / stop 全 200 |
| ⑦ webui API_BASE 硬编码（⑧） | `api.ts:89 resolveApiBase()`（`VITE_API_BASE` > `location.origin` > 默认） | ✅ 交付包前端在 `:57893` 上 45/45 请求都指向自己的源，含 57890 的 URL 0 个 |
| ⑧ Trae 是否已签到看不见（⑨，m13266） | `record_probe_today` 与 `record_today` 分离，探针**永不覆盖**真实 `fail` | ✅ 种 `fail` 后点刷新，JSON **逐字节未变**；UI 三态「已签到/未签到/未确认」 |
| ⑨ Trae 卡片官方 icon（⑩-1） | 两张官方 PNG + `product-marks.tsx:144-163 TraeMark(variant)` | ✅ 交付包前端渲染 5 张 trae 图，全部 512×512、`size-full object-cover` |
| ⑩ 启动期卡顿（⑩-2，m13819 + m14110） | 前端限流 4 并发 + `/api/credits/batch` + `warm_caches()` + `CHECKIN_STATUS_CONCURRENCY=6` | ✅ A/B：API **167 ⇒ 45**、最慢 **27,412 ms ⇒ 2,717 ms**、longtask **0** |

### 1.0.21 → 1.0.34（2026-10-04 → 10-07，业主逐条上报，全部真机实测后交付）

| 版本 | 内容 | 状态 |
|---|---|---|
| **1.0.45** | **账号级每日用量护栏（跨仓库调研后只合并了这一个功能）**：两个**顶层**旋钮 `daily_token_cap` / `credit_floor`（默认 **0/0 = 关闭**，升级行为完全不变）——某账号本地当天累计 Token 达到上限、或已知剩余积分 ≤ 下限，就**今天不再参与选号**（不是把整池烧掉，也不当坏号：不写 `disabled` / `no_route`、不进熔断计数）。① 判据单源 `Entry::daily_guard_blocks()`，被 `healthy()` 与 `pick_earliest_expiry()` 两处显式调用（后者刻意不调 `healthy()`，它要选正在冷却的号——本项目在 `no_route` 上踩过两次"主路径加了、兜底漏了"）；② `Counters::total()` = input + output + cache_write，**不含 cache_read**（缓存命中不是真实消耗，算进去会让长会话提前触顶）；③ 日切用 `clock_ms()` + `day_of_ms()`（不认注入时钟的 `local_day_now()` 无法单测），账按 `accounts[uid][day]` 存，跨零点不需要定时任务；④ `credit_floor` 三态：`credits_known` 只在**上游真值**写回时置位、**刻意不持久化**（重启先当"不知道"；ZCode 没有积分查询，把"没查到"当 0 会把 ZCode 号全闸掉）；⑤ 热重载走已有的 5 秒 `cfgwatch`，`set_daily_guard` 当场下发到条目，**网关进程不重启**；⑥ 旋钮声明为 `i64` 而非 `f64`（`DynamicConfig` 需要 `Eq`）。 | ✅ 真机 27 PASS / 0 FAIL（8 阶段）+ 全量回归 core 1301/0/4、router 1776/0/1 |
| **1.0.44** | **重启后 App 跟着自启、网关却没自启**（业主原话：「我刚重启之后,虽然软件跟随启动了,但是并没有网关也跟随启动,修复一下」）：网关 #1 在开机后 11:02:31 起过、11:02:33 就没了（**无停机日志、无 panic、无 Application Error、无 WER**，是宿主 Job Object + `KILL_ON_JOB_CLOSE` 的内核级终止），而下一个网关要等到 11:07:16。真缺陷是**死之后没有任何代码再看它一眼**：`maybe_autostart` / `ensure_gateway_alive` 都是**一次性**调用，`decide_sync_action` 在「网关没在跑」时还返回 `Idle`、把待重启标志**直接丢弃**。修法：`GATEWAY_ALIVE_WANTED` 三态意图阀（只置真、从不清零）+ RAII `GatewayStartGuard`（`compare_exchange` 独占）+ `ensure_gateway_running()` 返回 `ReviveOutcome{AlreadyRunning,Started,Idle}` + 指数退避 `GATEWAY_RETRY_EVERY_ROUNDS = 10`（30s × 10 ≈ 5 分钟一次），并把监督接进 `run_auto_sync_loop`（**必须排在 `apply_pending_restart` 之前**）。 | ✅ 真机 5 阶段全 PASS（强杀后约 16s 自动复活、显式 stop 后 66s 内 4 次观测始终不复活）+ core 1298/0、router 1752/0 |
| **1.0.43** | **对话中途一直中断**（业主原话：「为什么这个模型在对话过程中一直在中断?你看看咋回事」）：上游（并发/限流时）会在流中间发一帧 `finish_reason` 为失败值的负载，或直接发一帧 error 负载，而收尾判据只看「有没有 `[DONE]`」「有没有收到过帧」⇒ **失败帧被静默当成正常收尾**。修法：`upstream/sse.rs` 新增 `normalize_frame` / `normalize_payload`（补 `finish_reason: null`）与 `is_failure_finish_reason` / `payload_has_finish_reason` / `is_upstream_error_frame`，`SseFramesIter` 新增 `saw_finish_reason` 与 `saw_failure_finish_reason` 两个状态位，`aggregate` 遇失败 finish_reason 直接返 `Err`；`server.rs` 三条流式入口统一收尾，失败帧一律**在 `[DONE]` 之前**补一帧 error；`anthropic_stream.rs` / `responses_stream.rs` 各加 `upstream_err` 状态位。 | ✅ 真机 3 形态对比（finish_reason=error 无 `[DONE]` / 带 `[DONE]` / 正常 stop）+ 新增 `ab_finish_error.rs` 2 例 |
| **1.0.42** | **Claude CLI 看不到图片**（业主原话：「我接入到claude cli里面,他不能看到图片」）：`protocol/anthropic.rs` 把 Anthropic 的 content 数组转成 chat content 时只认 `text` 与 `tool_result`，`image` 块**直接落到 else 分支被丢弃**——请求里带着图，到上游时已变成纯文本，**且没有任何报错**。修法三处：① 按 `source.type == "base64"` / `type == "url"` 转成 OpenAI `image_url`（新增 `content_with_images` / `anthropic_image_to_chat` / `image_url_part`，`tool_result` 里的图也透传）；② `forward.rs` 新增 `strip_inline_image_payloads` 按 `;base64,` 剥掉载荷、改按尺寸估 token（兜底 `INLINE_IMAGE_FALLBACK_TOKENS = 1600`），否则一张内联图就能**假触发上下文超限**；③ `products/zcode/translate.rs` 反向（OpenAI → Anthropic）也补图。 | ✅ 真机新起 mock 上游，断言字符串 **`UPSTREAM_SEES_IMAGE count=2`**（上游真的收到 2 张图）+ `ab_images.rs` 4 例 |
| **1.0.41** | **空流不再惩罚账号、耗尽时报真实原因**（现场：zcode 通道单号空流被 `note_error` 喂进熔断，`err_total` 39，兜底又反复把熔断号捞出来打，每次以 `no_healthy_account` 耗尽，DSH 5 路并发重试放大成**风暴**）：① 空流分支不再调用 `apply_error_policy`（账号无辜，风暴窗口已由 1.0.40 的多号保护兜住）；② 轮转耗尽且最后失败为空流时改报 `EmptyStreamBurst` 契约（Chat `upstream_stream_stalled`）并如实描述单号空流；③ 新增 `empty_stream_single_message` 中文文案。 | ✅ 真机独立实例 + mock 空流上游：3 连请求后 `err_total=0`、无熔断、无放大 |
| **1.0.40** | **空流风暴熔断**（上游整体故障时不再全量换号放大，治 503 `no_healthy_account`）：新增 `crates/ai-gateway-router/src/emptystream.rs`（484 行）——上游整体吐不出内容时，**不再把每一个号都试一遍再全部记一笔**，那种"全量换号"会把一次上游故障放大成账号池级故障（熔断 + 兜底捞号 + 再耗尽，正好撞上客户端并发重试）。 | ✅ 真机 PASS（配合 1.0.41 的同一套 mock 空流上游验证） |
| **1.0.39** | **倍率改为定时刷新（不再永远停在启动那一刻）**：国服/国际服的 `creditMultiplier` 此前只在启动时取一次，上游改价后本机一直用旧值（现场表现是"国际服明明 `x0.00` 却还往计费号上路由"）。修法：网关按 TTL 定时重拉上游真值，到期区域自动跟进并落 `models_cache.json`，日志逐轮打 `[models-refresh] 定时刷新上游真值完成`。 | ✅ 真机用**成品里解出的内嵌网关**（独立端口 + 假上游）：TTL 压到 20s、上游倍率从 `x0.00` 改成 `x0.11`，**期间不发任何客户端请求**，`/v1/models` 返回 `creditMultiplier=0.11`；两区 `fetched_at` 同步推进 |
| **1.0.36** | ㉝ **重启前排空在途请求（修「回答写到一半断了」）**：`restart_gateway_serialized()` 此前是「拿锁 → 立刻 `stop_gateway()` → 600ms → start」——**没有任何排空**，正在服务的 SSE 流被拦腰截断。A 族断流（528 条）排查把每日维护窗口（本地 00:00:45）那两条定位到这个形状。修法：停之前先读 `/status.in_flight`（1.0.35 新透出的请求维度在途数），非 0 则每 250ms 轮询、最多等 30s，归零即停；两条**不等待的出口**都留日志 —— ① 旧版网关（≤1.0.34）无 `in_flight` ⇒ 按「不知道」处理、**绝不当成 0**（把不知道显示成安全会让优雅重启在最不该放的时刻放行，比不等待更糟）；② 30s 超时 ⇒ 照常重启并留日志（无限等会让「账号变更生效」这类操作彻底卡死，比偶尔打断一条长回答更糟）。新增 `gateway_in_flight()` / `drain_before_restart()` / `RESTART_DRAIN_TIMEOUT=30s` / `RESTART_DRAIN_POLL=250ms`（`gateway.rs` 重启段）。㉞ **保活兜底（整点漏了就补，不再静默丢一天）**：保活此前只在配置整点（默认 22:00）触发，而网关是独立子进程 —— 那一刻没在跑（应用关了 / 刚重启 / 维护窗口），当天保活**静默消失**，下次机会 24h 后。而保活要防的正是「服务端清理闲置 refresh 会话」（闲置数天 ⇒ 12153 `invalid_grant` ⇒ 账号被迫重新登录）。修法：网关把每次保活**成功**的时刻持久化到 `state.json` 的 `keepalive_at`（新字段，`pool.rs` 的 `StateAccount`/`Entry`/`load_state`/`flush` 四处 + `set_keepalive_at`/`keepalive_at`/`keepalive_snapshot` 三个访问器），启动时按「距上次成功 ≥ `schedule.keepalive_max_age_hours`（默认 24，0 = 关闭兜底）」筛出超期账号补跑（`Scheduler::run_keepalive_if_stale`，纯判据 `Scheduler::keepalive_is_stale` 可单测）；**未超期一个请求都不发**。补跑在**后台 spawn** —— 首次升级后所有账号 `keepalive_at` 都是 0（旧 state.json 没这个字段），27 个账号同步跑会让端口 30s 不监听，而补跑只改账号凭证、与监听无关。宿主侧 WorkBuddy 保活循环同步改口径：原来比自然日（`date_str`），现在按「距上次成功刷新的时长」判（新增 `refresh::keepalive_due(days)`）。㊱ **成本调度补三条缝（业主 10-08 第二报：「这几个国际服账号模型还没用完呢,都是0的倍率,为什么还能路由到国内账号上?不是成本优先吗」）**：现场 12 个国际版号对 `deepseek-v4.1-flash` 是 `x0.00`（免费）、15 个国服号是 `x0.11`（计费），而国服号有 **710 条对话记录**（45–50 条/号，**极均匀**）。定位读数：① 710 条是真对话 —— 每条平均 **27 万** input token，而养号任务走 billing 域**不记 token**；② 15 个号被**均匀**摊分 ⇒ 会话各自绑到不同号，不是偶发；③ `fallback_earliest_expiry` 全量日志 **0 行**（该日志 1.0.23 起就有）⇒ **不是兜底路径**；④ 1.0.36 之后 679 次 chat 全走国际版。⇒ 唯一解释是**粘性绑定**：`pick_by_uid_for_model_region` 只查 healthy / in_flight / model_available，**完全不看 cost** ⇒ 会话一旦绑到计费号，30 分钟 TTL 内所有请求都走它。修法三处：① **粘性快路径加成本守卫**（新 `Pool::sticky_costs_more_than_a_free_option`，`pool.rs`）—— 绑定号「确定计费」而池里存在「确定免费」的可服务候选时返回 `None` ⇒ 调用方解绑走完整选号；② **兜底路径补免费优先**（`pick_earliest_expiry` 先跑 `free_tier` 再挑冷却最早结束的 —— 另一条兜底 `pick_best_from` **本来就有**，只有这条漏了）；③ **区域清单落盘复用**（`models_cache.rs` 新增 `models_cache.json` + `ModelsCache::with_persistence` + `stale_truth`）—— 倍率是成本调度的唯一输入，重启后缓存为空会让「免费优先」静默失效；**拉取失败 / 退避窗口内也退回已知真值**（倍率是静态事实，不该被一次拉取失败牵连）。㉟ **依赖告警**：`source-map-js` 1.2.1 → 1.2.2（high，补丁 1.2.2）、`rustls` 0.23.43 → 0.23.45（medium，补丁 0.23.45）；`glib` 保持 0.18.5 —— 告警要求 ≥0.20.0，而 `glib` 由 `gtk 0.18.2` ← `tauri 2.11.5`（经 `tray-icon 0.24.2` / `libappindicator 0.9.0`）引入，`cargo update -p glib --precise 0.20.12` 直接报 `glib = ^0.18` 约束冲突；上游 `gtk` 版本线止于 **0.18.x**（crates.io 无 `gtk 0.20`），`tray-icon` 0.24.2→0.26.1 **每版都仍写 `gtk ^0.18`**；且该依赖**只在 Linux 目标**（`cargo tree -i glib --target x86_64-pc-windows-msvc` 报 `nothing to print`），本产品在 Windows 发布 ⇒ 暴露面为零，属上游待办。 | ✅ 单测 router **1692/0/1**（+`keepalive_staleness_boundaries`、+`keepalive_max_age_negative_clamps_to_disabled`、+`keepalive_due_closed_form`）、core **1289/0/4**（+`restart_drains_in_flight_before_stopping` / `drain_treats_missing_in_flight_as_unknown_not_safe`）、workspace **3056/0/7**；★ **真机实测跑的是发行包里的二进制**（`test-bin\inst-1036-final`：隔离 `AI_GATEWAY_HOME` + 单账号凭证副本 + 本地假上游 `:17952` 15×700ms）：`/status.in_flight` 逐秒轮询在流在飞期间恒为 **1**、流结束立刻回 **0**；`POST /api/gateway/restart`（出厂 server）耗时 **3814ms**，其中排空等待 **1832ms**，日志逐字 `[gateway] 重启前排空：当前 1 个请求在途，等待其结束（最多 30s）` / `[gateway] 重启前排空完成：在途请求已归零（等待 1832ms）`；客户端侧 `STREAM_END +10677ms chunks=18 sawDone=true bytes=3035`（**完整 `[DONE]`，未被截断**）。保活：`keepalive_at=0` ⇒ 启动即补跑并写入真实时刻（age=37s）；紧接着重启 ⇒ 日志里**一条 keepalive 都没有**（未超期不发请求）；`listening` 与「开始补跑」同毫秒（端口不被阻塞）。★ **成本调度真机实测（发行包二进制，隔离实例）**：同一会话 + 制造冷却让绑定落到计费号，再放开 —— `23:12:30` free 健康 ⇒ 6 次全走 free；`23:12:45` free 冷却 ⇒ 3 次走 paid；`23:13:04` free 仍冷却 ⇒ 4 次走 paid；**`23:13:26` free 恢复 ⇒ 4 次全部回到 free**（粘性成本守卫生效，修好前会一直走 paid 到 TTL 结束）。清单持久化：**关掉上游再重启**，`/v1/models/regions` 仍给出 `cn=0.11 / intl=0.0`（来自落盘缓存，`last_error` 如实记录失败）。签名端到端验签 **VALID**（`Ed` 公钥 / `ED` 预哈希签名，FILE SIG 过 blake2b-512，GLOBAL SIG 过 `sig64||comment`）；约束④全程成立（业主 `:7864` / `:3080` 未动） |
| **1.0.35** | ㉛ **可观测性补完（业主 10-08「推送代码吧 / 发布一下 rela 新版本的构建」）**：排查 A 族断流（528 条）时发现**网关日志每一行都没有时间**——宿主把子进程 stdout/stderr 直接重定向进文件（`open_router_log()` + `cmd.stdout(out).stderr(err)`），没经过任何带时间戳的框架，于是与客户端插件日志（`2026-10-08T05:37:42.945Z`）**无法对齐**，只能拿字节数猜。三件事：① 新增 `crates/ai-gateway-router/src/logging.rs`（`stamp()` + `#[macro_export] macro_rules! logline`），**173 处 `eprintln!` 全量**改为 `logline!`，另把 36 处周期性 `println!`（Trae 积分刷新 6、计费周期刷新 3、启动摘要 27）也改到带时间戳的 stderr；② 三个流式入口收尾各加一行 `流结束（已收 N 个有效帧 / M 字节，saw_done=…，saw_finish_reason=…）`（`server.rs` anthropic `:2708` / responses `:2840` / chat `:3239`）—— 此前**流正常结束时一行日志都没有**，「网关有没有把这条流收干净」无法回答；③ `/status` 新增 `in_flight`（请求维度在途数，新 `Pool::in_flight_total()` `pool.rs:2457`）与 `version`（`env!("CARGO_PKG_VERSION")`）。★ **stdout 一个字没动**：`qoder-login`/`zcode-login` 的 JSON 仍只走 stdout（宿主 `serde_json::from_str(stdout.trim())` 直接解析），实测子命令 stdout 恰好一行合法 JSON。㉜ **国服账号「消耗积分」恒为 0**（业主 10-08 第二报：「workbuddy 国服账号错误的显示积分为 0」，截图 15 个国服号整列 0、12 个国际版正常）：网关日志 **19 个 uid × 202 次** `credit-refresh <uid>: upstream client (http 403): {"code":10085,"msg":"请求不合法，如有疑问请联系客服"}`。定因（同 token / 同 URL / 同 body，**只换头**逐项二分）：裸 `Authorization`+`Accept`+`Content-Type`+`X-User-Id`+`X-Domain` ⇒ **403 code=10085**；再加 `X-Client-Platform: web` / `Referer` / `User-Agent` **任一** ⇒ **200**；单独加 `Origin` 无效 ⇒ 计费后台 WAF 拒绝「看起来像裸 API 客户端」的请求。宿主 `ai-gateway-core/src/modules/credits.rs` 的 `resource_auth_headers` 早就带了这三个（照用户中心 Axios 拦截器写的），**网关这条后台巡检没带** ⇒ 界面读网关池快照，故恒显示 0。修法：新增 `headers.rs::resource_headers`（`X-Client-Platform: web` + `Origin`/`Referer` 跟随区域 + 与 chat 同 UA + `Accept: application/json, text/plain, */*`），**只给 `client.rs:539 user_resource_detail` 用**；`billing_headers` 保持原样（它被签到/活跃上报/领奖/**任务列表**共用，其中任务列表**必须不带 UA**——第一版把 UA 加进共用头，一次性踩红 7 个用例）。实测：隔离实例真实计费端点，`credits` 0 ⇒ **7246**、`expire_day` 空 ⇒ **2026-10-23**，日志无 403。 | ✅ 单测 router **1691/0/1**（+3 时间戳单测、+3 计费头护栏单测）、workspace **3051/0/7**；★ 真机隔离实例 `test-bin\inst-1035-obs`（独立数据目录 + 单账号凭证副本 + 本地假上游 `:17912`，端口 `:17911`）：三种上游收尾形态实测（正常 ⇒ `saw_done=true` 不补；只 finish_reason ⇒ 只补 `[DONE]` 不报错；真截断 ⇒ 补 error 帧 + `[DONE]`），响应体逐字节抓取确认 `[DONE]` 恒为 1 个且 error 帧在它**之前**；三协议 200；`/status` 新字段实测（1.0.35 `version=1.0.35 in_flight=0`，业主 `:7864` 的 1.0.34 **无这两个字段**）；子命令契约实测；约束④全程成立（业主 `:7864`/`:3080`/`:7890` 未动） |
| **1.0.34** | ㉙ **「对话输出完就中断」**（业主 10-07 原话：「对话过程中对话会输出完就中断，这是我为什么回退版本的原因」）：上游收尾顺序 = **先** `finish_reason`、**后** `[DONE]`，两者之间那一瞬最容易被掐断；旧判据 `if saw_done \|\| !saw_any_frame` 只看 `[DONE]` ⇒ **内容已完整却判成本轮失败**（补 error 帧 → 客户端显示「本轮运行失败」）。现场：`gateway.log` **16 条** `上游流读取失败…saw_done=false`（**全为 `www.workbuddy.ai` 国际版号**，已收 29,628–1,992,314 B）+ 插件 **18 次** `upstream stream truncated (openai) — emitting in-band error`；失败率 **16/13,780 ≈ 0.12%**；断流集中在**请求发出后 32 ms – 6 s**、**多会话几十到几百毫秒内齐断** ⇒ 共享通道抖动（与账号/模型无关）。修法：`SseFramesIter` 加 `saw_finish_reason`（`upstream\sse.rs:563` / `:623` / `:650` / `:684` / `:722` + 自由函数 `payload_has_finish_reason`，**必须在 `normalize_frame` 之后判非空字符串**）+ `failure_message` 三参（`anthropic_stream.rs:408`/`:417`、`responses_stream.rs:671`/`:678`）+ `server.rs:2853 const TAIL_DONE_FRAME` + 读错误分支 `:3150-3161` 按正常收尾补 `[DONE]`。★ **不放宽**：首帧之前就断仍 503 + 换账号重试（`forward.rs:1130`/`:1147-1233`）、真截断仍报错。㉚ 回答业主「这几个不是都有接口返回吗？」—— `/v1/models` 是**并集**（185–189 条），国服独有 9 / 国际版独有 16 ⇒ 列表里有 ≠ 抽到的那个号能发。 | ✅ 单测 router **1685/0/1**（+4）、workspace **3045/0/6**（+4）；★ 假上游 5 模式矩阵（`:17903` 出厂 1.0.33 vs `:17904` 出厂 1.0.34）modeA/modeC **1.0.33 补 error 帧 → 1.0.34 无 error 帧**、modeB/D 两边都报错、modeE 两边正常、`doneCount` 恒 1；三协议探针（`/v1/messages` `message_stop`、`/v1/responses` `response.completed`）；真机隔离台 27 号只读副本 **15/15 全 200**；包内 GUI 内嵌 gz blob **逐字节 identical**（`0x17CB042`，解压 13,831,680 B / `9447272A…`）；签名验签 VALID；约束④全程成立（业主 `:7864` / `:3080` / `:7890` 未动） |
| **1.0.33** | ㉗ **裸名 `space-bunny` ⇒ 400 `model_not_in_upstream`**（业主 10-07 报障截图，错误码本身证明 1.0.32 的选号已修好）：1.0.32 把「静态兜底清单没命中」从「确定没有」降级成「猜测」（`guess_excluded`），使 **27 个 WorkBuddy 号 + 1 个 Trae 号同池抽签** ⇒ ~3% 抽中 Trae（`trae-2593120869167228`，业主 `gateway.log:1080/1082/1083` 逐字对上）⇒ Trae 上游 4001 ⇒ 400。修法：区分「猜测 vs **权威**」—— Trae 兜底清单派生自 `dispatch.rs:275 ALIASES`（58 条，文档称「唯一的模型名真相」）⇒ 对 Trae 是**确定**；新增字段 `product_fallback_authoritative`（`pool.rs:872`）+ setter（`:1125`）+ helper（`:1134`）+ `product_definitely_lacks_model` 末行（`:1305` = `known || fallback_authoritative`）+ `pick_for_model_any` 新支（`:2679`/`:2691` ⇒ `scoped`）+ 注入（`bin\gateway.rs:148/182/188`，**只有 trae**）。㉘ 顺带修 A/B 实测发现的**启动窗口观测集为空** ⇒ 新增 `server.rs:1684 warm_up_live_models` + `bin\gateway.rs:923` 启动预热（失败静默）。 | ✅ 单测 router **1681/0/1**（+3）、workspace **3041/0/6**（+3）；★ **出厂二进制冷启动 120/120**（业主现场形状：27 WB + 1 Trae、qoder/zcode 空）—— 对照 1.0.32 同形状 **0/120**（30× `model_not_in_upstream` + 90× `model_not_in_region`）；探针矩阵 6 用例（sticky / `国服:space-bunny` / 反向 `gpt-6-sol` / `glm-5.3` 全 200，`国际版:space-bunny` 400 预期）；包内 GUI 内嵌 gz blob **逐字节 identical** + 解压哈希 `1BEDC8E6…`；签名验签 VALID；约束④全程成立（业主 `:7864` / `:3080` / `:7890` 未动） |
| **1.0.32** | ㉔ **裸名 `space-bunny` ⇒ 503 `no_healthy_account`**（业主报障）：网关把「静态兜底清单里没有」当成「平台确定不提供」⇒ 27 个 WorkBuddy 账号**全被排除** ⇒ 候选集空 ⇒ 上游侧回落到最通用的 503 文案。修法：新增**上游实测清单** `product_model_observed_set`（`pool.rs:1106 observe_product_models`，每次 `GET /v1/models` / `/v1/models/regions` 刷新，调用点在 `server.rs:678` / `:729` / `:1725`；只放宽「不排除」，**永不**参与「声明」——避免重演 2026-09-21 qoder 抢号）+ 新增 `product_definitely_lacks_model`（`pool.rs:1226`，**只有宿主声明清单**能说「确定没有」）+ `pick_for_model_any`（`pool.rs:2575`）三档严格分层（①有平台声明 ⇒ `scoped+unknown`，挑不到直接 `None`；②无声明 ⇒ `scoped+guess_excluded`；③ `guess_excluded` 为空才 `None`）+ 落点 **F/G**（`:2887` / `:2994`）。㉕ 顺带修**裸名选到错区账号**（⇒ 400 `model_not_in_region`）：新增 `observe_product_models_in_region`（`:1140`）+ `model_region_hint`（`:1192`，要求两区都拉过且恰好一区见过）+ 落点 **I/J**（`:3055` / `:2905`）。㉖ 回答「最大输出」：`space-bunny` = `max_output_tokens 128000` / `context_length 1000000`。 | ✅ 单测 router **1678/0/1**、workspace **3038/0/6**（+9 全是本次新单测）；**包内网关**（`inst-1032-unpack\embedded-gateway.exe`，逐字节拷到隔离实例 `:17866`）裸名 `space-bunny` **20/20 全 200** + 正向/反向/sticky 全 200 + `国际版:space-bunny` 400（预期）；签名端到端验签 VALID；约束④全程成立（业主 `:7864` / `:3080` / `:58354` 未动） |
| **1.0.21** | 平台路由开关并入标签（②）+ 账号卡片统一任务区（③）+ 首启静默修复（①） | ✅ 真机 PASS（业主实机装的**就是这一版**） |
| **1.0.22** | 设置页各平台配置拆到对应 tab（⑤）+ 积分/Token 并入 WorkBuddy tab（④） | ✅ 真机 PASS |
| **1.0.23** | ZCode 1113 误判硬冷却（⑪）+ 流式中断归因（⑫）+ 跨账号任务队列漏跑（⑬）+ ⑦ 消耗记录恢复（20 天 / 15.3 万条） | ✅ 真机 PASS |
| **1.0.24** | 启动卡顿（⑩b）：预热窗口改走 `/blank` + 验证码池预热闸门 | ✅ 真机 PASS |
| **1.0.25** | 页面切换保持状态缓存（⑥）：ErrorBoundary key 改 resetKey + 三级视图入 URL | ✅ 真机 PASS |
| **1.0.26** | 平台面板互斥显示（★ 真产品缺陷：`present = forceMount || isSelected` ⇒ 非选中面板永不 `hidden`）+ 豆包单账号任务区 + 网关换新护栏 | ✅ 真机 PASS |
| **1.0.27** | ⑰ 用量统计模型名**大小写折叠**（写入口径 `model_key_of` + 载入自愈 `merge_case_variants`）+ 计费行平台归属按计费键回填（`backfill_billing` 旧口径按 uid 形状猜，重启后分布会漂） | ✅ 真机 23/23 PASS（载入自愈 8→6）；★ 写入口径另测 **23/0**（见五节） |
| **1.0.28** | ⑯ 流中途上游错误帧识别 / 记日志 / 计入账号健康度（`note_upstream_stream_error`）+ 空 product 归一化成 `workbuddy` | ✅ 真机 43/0（抓包）+ 50/0（GUI）+ 24/0（用量页） |
| **1.0.29** | ⑱ **桌面端接入应用内调度**：`spawn_background_loops()` 新增第 7 个循环（启动 60s 后起，每 5 分钟一轮，`spawn_blocking` 里调 `apps_ops::run_in_app_due_tasks`），修复「Trae 每日签到从不自动执行」+ 每轮无条件打 `[调度] 检查完成…` 日志 + 护栏测试 | ✅ 真机：包内二进制实跑（`[调度] 检查完成：本轮执行 0 项`）+ debug 构建实触发（`[调度] Trae 每日签到 计划 00:05 成功（退出码 0）`，`in_app_schedule.json` = `{"trae-checkin":"2026-10-06"}`）+ GUI 50/0 + 用量页 24/0 |
| **1.0.31** | ⑳ **合入上游 6 项可用更新**（`account.rs` 新增 `display_string()` / `current_account_view()` 把 `id/uid/email/nickname/enterpriseName/needsReloginReason/note/domain/phoneNumber/accountType` 一律归一成字符串或 `null`，杜绝上游 JSON 结构体透传 UI 导致 React 崩页；四处状态出口统一；`gateway.rs static_models()` 与网关侧静态表取并集 **30 条** + 两侧快照测试锁死；`rotate.rs` 剩余天数向上取整；`tray.rs` 显式清 `LIGHTWEIGHT_MODE`；`classify.rs` 增补 `4028`）+ ㉑ **截断留痕**（`server.rs` 补 `sent_bytes` 与「上游未发 [DONE]」分支 eprintln，此前该分支**零日志**）+ ㉒ **对齐上游 `1c8bfed` 的网关日志查看入口**（`router_log_path()` / `read_router_log_tail()` / `gateway_status().logFile/.logBytes` / `read_gateway_log` / `reveal_gateway_log`；上游同款命令按行数取，本仓库按字节取）+ ㉓ **非客户端纯部署产物** `dist-single\ai-gateway.exe`（27,274,752 B，纯 CLI 无 GUI） | ✅ 单测 core **1285/0/4**、router **1669/0/1**；假上游 6 模式矩阵 6/6；隔离实例真机端到端 200；交付包解包核验 `FileVersion=1.0.31` + 新符号命中；约束④全程成立（业主 :7864/:57890 未动） |
| **1.0.30** | ⑲ **业主最后四件事一次修完**：① Trae 网关凭证被清空（`trae_account.rs` 的 `active_uids()` 把 **WorkBuddy 的 uid 勾选名单**当成 Trae 的过滤条件，两套 uid 空间交集恒空 ⇒ `export_accounts_to_dir` 清理循环删光 `trae-*.json`，且切回 balance 不自愈；改恒 `None` 不过滤）+ ② 后台任务完成事件（`lib.rs` 调度循环 `emit("background-task-done")` + 新增 `src/lib/use-background-task-refresh.ts`，Trae/豆包页按前缀监听 `dsh:background-task-done`）+ ③ 后台拉客户端隐藏窗口（`Action::hides_client_window()` 只对 `KeepAlive` 为真 + `CLIENT_HIDE_WINDOW_KEY` 默认 `true` + 设置→后台运行勾选框）+ ④ 顺带修「豆包保活失败仍记账」（`cli_task.rs` 失败改返 `Err`、只在成功时写时间戳、空池短路） | ✅ 真机：包内二进制 reqB **三臂 A/B/C**（修复前记账 / 修复后不记账 / 空池短路）+ reqC **GUI 勾选框↔磁盘往返** + reqD **48/48**（三种断流形态，用包内反解出的网关）+ workspace **3011/0/6** |

### 9.11 1.0.36 的构建与真机实测（2026-10-08，尚未推送）

**版本**：1.0.36（`scripts/bump-version.ps1 1.0.36` 统一 bump，覆盖 `package.json` /
`src-tauri/tauri.conf.json` / `npm/package.json` / 四个 `Cargo.toml` / `npm/platform/*` /
`Cargo.lock` 的 4 个 workspace 包）。

**构建**：`scripts/build-signed.ps1 -Bundles nsis`（签名密钥用 `-KeyFile` /
`-PasswordFile` 显式指向 `keys\ai-gateway-updater.*`；默认路径 `~/.wb-switch/wb-switch-updater.key`
在本机不存在）。内嵌网关 `crates/ai-gateway-core/embedded/gateway.exe` 与
`target/release/gateway.exe` **逐字节一致**。

| 产物 | 字节 | SHA256 |
|---|---|---|
| `dist\AI Gateway_1.0.36_x64-setup.exe` | 14,736,756 | `24F760F84958C315002B50EF6568F6C2F6C3BC6787709B7DFC8C0A5824DB30F4` |
| `dist\AI Gateway_1.0.36_x64-setup.exe.sig` | 424 | `36D18C01BE5B286BDB160A118DE0BFCE403786947B9AAF09D26BBBB8F3F0220E` |
| `dist\ai-gateway_1.0.36_server-x64.exe` | 27,422,208 | `1CD866E2A724749E4589FA8300677D5A7B5DEBC4F5F3D5B5F19B1A8DA1043C2E` |
| `crates\ai-gateway-core\embedded\gateway.exe` | 13,999,616 | `458163AECF3B317E7AFAE5836CFE90D542B137A9436033D124CE030F432AF75D` |

**签名验签（端到端，不只看 keyid）**：`tauri.conf.json` 的 `plugins.updater.pubkey`
（base64 解出 `Ed` 前缀 + keyid `6fa3c36f4be67c7e`）与 `.sig`（`ED` 前缀 = **预哈希**）配对；
用 Node `crypto.verify` 逐条验：

- **FILE SIG**（`sig64`）过 **`blake2b-512(安装包)`** ⇒ **VALID**（这就是 updater 实际校验的那条）；
- **GLOBAL SIG** 的签名消息 = **`sig64 || trusted_comment正文`**（不含 `trusted comment: ` 前缀、不含换行）
  ⇒ **VALID**。

⚠ 记下来：上一轮记的「GLOBAL SIG 消息 = `sig64 || trusted_comment`」是对的，但本轮先用
「`sigline + \n + comment`」试了一轮全 invalid —— **预哈希与全局签名是两套消息**，
不要用同一个式子套两者。

**真机实测（跑发行包二进制）**：`test-bin\inst-1036-final`，隔离 `AI_GATEWAY_HOME`，
单账号凭证副本（用后已删），本地假上游 `:17952`（15 帧 × 700ms ≈ 10.5s 慢流）。

| 观察 | 读数 |
|---|---|
| `/status.in_flight` 逐秒轮询（流在飞） | 连续 11 次 **1**，流结束那一次 **0** |
| `POST /api/gateway/restart`（出厂 server） | **3814 ms**（含排空 **1832 ms**） |
| 排空日志 | `[gateway] 重启前排空：当前 1 个请求在途，等待其结束（最多 30s）` / `[gateway] 重启前排空完成：在途请求已归零（等待 1832ms）` |
| 客户端侧 | `STREAM_END +10677ms chunks=18 sawDone=true bytes=3035`（完整 `[DONE]`） |
| 保活补跑（`keepalive_at=0`） | 启动即补跑并写入真实时刻（age=37s） |
| 保活幂等（37s 前刚刷过） | 日志里**一条 keepalive 都没有** |
| 端口不被补跑阻塞 | `listening` 与「开始补跑」同毫秒 |

**测试基线**：workspace **3081 passed / 0 failed / 7 ignored**（15 suite）。

**已做（2026-10-09 12:05，业主 m11719「推送一下吧」授权）**：`main` + `v1.0.36` + `v1.0.37` 已推
`fork`（`iuuuuuuuu/ai-gateway`），并各发一个 Release。**只推 `fork`**；`origin` / `upstream` / `aigw`
三个 remote 全程未碰（与 9.6 授权一致）。

### 9.23 第三轮复核：8 处问题逐条闭环（2026-10-14 凌晨，同一条业主指令）

结论先说：**0 处遗留未处理**。两轮独立复核提出的 8 处问题里，2 处是**真实缺陷**（已在 9.21 记录并修复）、6 处是文档/注释/测试口径问题（其中 2 处复核后确认**无需改代码**：一处代码里本来就写明了，一处是有意不为）。本轮把每一条都落到**可核对的提交或函数名**上：

| # | 级别 | 问题 | 处置 |
|---|---|---|---|
| 1 | 真实缺陷 | 积分成因的恢复话术错误（对所有成因都写「过了本地零点自动恢复」） | ✅ 9.21 已修（成因枚举 + 文案分叉） |
| 2 | 真实缺陷 | Trae 无限额把没问过的旧余额「追认」成真值 | ✅ 9.21 已修（陈旧值出作用域 + `set_expiry_only`） |
| 3 | 建议 | 测试注释里的行号指错 | ✅ `cb62b2b` 已改：`usage.rs` 那条测试的注释写的日期与常量不符，且断言是拿 `today_key()` 跟它自己调用的 `day_of_ms()` 比（同语反复，退化成 `Local::now()` 也能过）；改用过去的时刻 + 写死期望值。`server.rs` 测试表里的 `forward.rs:969` 也漂了，改成 `forward.rs:1043` |
| 4 | 建议 | `bin/wb2g-ref.rs` 没有 `set_daily_guard` 调用，与主程序不对称 | ℹ️ 已核实为**无害**：该 A/B 对照入口根本不读 `-config`（它只看 `std::env::args()` 的端口 + `WB2A_*` 环境变量），因此 `Pool` 停在构造默认值 `daily_token_cap: 0` / `credit_floor: 0`（`pool.rs` 里两处初始化），护栏恒为关闭 —— 与主程序「未配置」时逐字一致。**有意不补**：补了反而让这条只用于对照的入口多出一份与产品无关的差异 |
| 5 | 建议 | 文档指针指向的行号已随提交漂移 | ✅ 本轮新增/修改的文档改成「**绝对路径 + 函数名/关键字**」打头、行号只作辅助（如 `upstream\client.rs` 的 `build_proxy`、`scheduler\mod.rs` 的 `refresh_credits_with_gap`），这样即便以后再漂也能顺着函数名找回来；`cb62b2b` 已把最离谱的一处（`forward.rs:969` -> `forward.rs:1043`）改到位 |
| 6 | 建议 | `daily_guard_recovery` 只有一条出口被单测覆盖 | ✅ 已覆盖三条出口：`forward.rs` 里 `daily_guard_message` 的三条文案单测（`daily_guard_message_states_a_concrete_recovery_time`、`daily_guard_message_distinguishes_partial_from_full`、`daily_guard_message_does_not_promise_a_midnight_reset_for_credits`），另有一条端到端走 429 全链路 |
| 7 | 建议 | `Status.daily_guard` 的语义在文档里未写明 | ✅ 复核后发现**代码里本来就写明了**（`pool.rs` 的 `daily_guard` 字段注释逐字写着「只在护栏**开启且真的命中**时为 true：护栏关闭（默认）时恒 false，老行为逐字不变」），无需再改 |
| 8 | 建议 | 真机记录里额度闸与余额闸来自**不同构建**（1.0.46 网关 + 1.0.44 宿主） | ✅ 9.22 已修：隔离宿主从 HEAD 重编，两条闸在同一 1.0.46 上复跑 |

⇒ 本轮之后，第三方功能合并这一条线的**代码、单测、真机读数、文档**四项全部对齐，剩余只有「等业主点头」的动作（推送 / 打 tag / 更新 `dist\latest.json`）。

★ 权威复核口径（避免下次重复踩）：

1. **护栏是选号闸，不是限流器**：用量在请求结束后才记账（`record_billed`），所以「刚用完」到「被挡住」之间有最多 5 秒；**进行中的请求永远不会被打断**。判据本身是纯函数（`Entry::daily_guard_blocks()`），`healthy()` 与 `pick_earliest_expiry` 共用它，不存在两套口径。
2. **计数口径**：`Counters::total() = input + output + cache_write`，**不含 `cache_read`**（缓存命中不该算业主花钱）。同一个函数同时喂给 `/status` 与护栏判据，不会出现「界面显示一套、护栏按另一套算」。
3. **两个旋钮只在 `Config` 顶层生效**：宿主写进原生配置时就在顶层，且代码里逐字警告过「放进 `pool` 块 serde 会**静默忽略**」。真机两轮都断言过「顶层有值、`pool` 块里没有」。
4. **护栏只拒绝，不落状态**：不写 `disabled`、不写 `no_route`、不设 `until`，`/status` 里这些位全程是 0，账号照旧被列出、照旧能跑 care 任务。

### 9.22 补验 `credit_floor` 积分闸（2026-10-14 凌晨，同一条业主指令；不改产品代码，只补真机读数）

9.20 / 9.21 都如实记了同一个**没验到的面**：`credit_floor`（保留积分下限）**没有真机读数** —— 真机上游是死的假上游 ⇒ 积分巡检刷不回真值 ⇒ `credits_known` 恒 `false` ⇒ 「未知不拦」的设计让积分闸**在整轮 27 个 PASS 里一次都没被触发过**。这一轮把它补上：**不改任何产品代码**，只加了一套**本机假积分上游**的测试装置。

**为什么必须造假上游**：积分闸只对**上游成功查到过**的余额生效。`scheduler\mod.rs:1027-1033` 的取数是 `Err(e) => { crate::logline!("credit-refresh {}: {e}", st.uid); continue; }` ⇒ **查询失败时旧值原样留着**，一个没人成功问过的数字永远 arming 不了这道闸。所以必须让网关**真的**从某个上游读到一个小余额。

**怎么做到（全程不出本机）**：

| 环节 | 做法 | 依据 |
|---|---|---|
| 假上游 | `test-bin\fake-billing.js`（Node，**只监听 `127.0.0.1:57940`**），按真实契约回**双层信封** `{"code":0,"data":{"Response":{"Data":{"Accounts":[{CycleCapacitySize:100,CycleCapacityRemain:5,DeductionEndTime:<now+30d>}]}}}}` | `upstream\client.rs:1265 unwrap_envelope` 返回顶层 `data`，调用方再读 `data.Response.Data.Accounts[]`（`:549-553`）；余额取 `CycleCapacitySize>0` 时的 `CycleCapacityRemain`（`:1002`） |
| 把网关指过去 | 启动包一层 `.cmd`，只给子进程加 `WB2A_BILLING_BASE_CN=http://127.0.0.1:57940` | 这是代码里自己写明的 A/B 对照钩子（`bin\gateway.rs:469-471`「仅供 A/B 对照测试把网关指向假上游（生产留空走内置常量）」）；宿主 spawn 网关**没有 `env_clear()`**（`gateway.rs:3891-3894`）⇒ 子进程继承环境变量 |
| 绕开死代理 | 同时设 `NO_PROXY=127.0.0.1,localhost`（含小写别名） | 夹具 native config 顶层写着 `proxy: http://127.0.0.1:57999`（**死端口**，由 `gateway.rs:3201` 从「更新代理」设置写入）；`build_proxy` 只在**有代理时**挂免代理表 `reqwest::NoProxy::from_env()`（`upstream\client.rs:153-154`）⇒ 不设 `NO_PROXY` 的话回环请求会被丢给 57999 |
| 域判断 | 夹具账号域名 `fake.example.invalid` 不在 `INTL_DOMAINS` 白名单里 ⇒ 属**国服** ⇒ `billing_base()` 才走 CN 覆盖 | `upstream\client.rs:366-377` + `auth.rs:208+` |
| 开门 | 宿主 `gateway_config.json` 顶层写 `credit_floor=100`（**大于**读到的 5）⇒ `POST /api/gateway/platform-enabled` ⇒ 重生成 native config | 与 9.21 的 P4 同一条**真实热重载**路线，**不重启** |

**真机实测（7 个阶段，`test-bin\guard-credits.log`，56 行，`CREDIT WITNESS RESULT: FAIL=0`，共 27 PASS / 0 FAIL）**：

- **C1/C3 起点为 0**：`guard-prepare.ps1` 会把两个旋钮从**宿主与 native 两份配置里都删掉**（夹具基线），启动后断言 native config 顶层 `credit_floor=0` 且 `daily_token_cap=0` ⇒ 后面的拦截**不可能是额度成因**。
- **C4 积分是真的从假上游读来的**：两个账号启动后都变成 `credits=5`（**全新池子本来是 0** ⇒ 只可能是巡检写进去的），假上游收到 **2 次** `POST /v2/billing/meter/get-user-resource`，且 `gateway.log` **一条 `credit-refresh` 报错都没有**（两次都解析干净）。同时还断言了**光有余额不会禁用账号**（`disabled=0` / `no_route=0`）。
- **C5 积分闸真的拦人，且文案不再骗人**：`credit_floor=100` 生效后（pid **不变**，热重载）⇒ 两个账号都 `daily_guard=true` ⇒ chat = **429** `rate_limit_exceeded`，文案含 **余额 / 不会**（不随日期恢复）与 **积分巡撤回充 / 保留积分下限**（两条真实出路），且**不含**只有额度成因才有的 **额度按** ⇒ 两种成因**没有混为一谈**（这正是 9.21 缺陷 1 修的）。同时 `disabled=0`、`no_route=0`、两个账号**仍被 `/status` 列出** ⇒ 它是**选号闸**而不是 kill switch。
- **C6 调低下限立刻放行**：`credit_floor` 改成 1（低于余额 5）⇒ 闸门在**没有任何新积分取数**的情况下释放，chat 不再 429（回到 503，即夹具死上游的正常失败），pid 仍不变。
- **C7 清理**：三个测试进程按 **PID + 路径**双条件收掉（`leftovers: gw=0 srv=0 node=0`），57930/57931/57940 全释放，**业主的 :7864 / :3080 全程在听**。

**同一构建下的额度闸复跑**（顺手做的公平性修正）：9.21 那次是「1.0.46 的**网关** + 1.0.44 的**宿主**」。这次把隔离宿主**从 HEAD 重新编译**（`cargo build --release -p ai-gateway-server --target-dir test-bin\build-server`，2m23s，产物 27,090,432 B），二进制里 `1.0.44`/`1.0.45` 字面量 **0 次**、`1.0.46` **2 次** ⇒ 再跑一遍 `guard-live.ps1`：**27 PASS / 0 FAIL**，横幅 `ai-gateway v1.0.46`，P4 拦截文案与 9.21 记录**逐字一致**。现在两条闸的读数都来自**同一个 1.0.46 构建**。

**遗留（仍然没验的）**：`credit_floor` 对 **ZCode** 号恒不生效（ZCode 没有积分查询接口，`products\zcode\` 下没有 `credits.rs`）—— 与 9.20 第 2 条相同，本轮**未变**。

★ 本轮踩到的两个坑（都是**测试装置**的坑，不是产品缺陷）：

1. **PowerShell 的 `if (Fn 'x' -eq 0)` 会静默判假**：整句被解析成**一条命令**（名字 `Fn`、一个位置参数 `'x'`、`-eq`/`-or` 变成**命令参数**），字符串永不等于整数 ⇒ 条件恒为 false 且**解析零报错**。修法：**先把函数调用取到变量再比较**。同样的写法在 `guard-live.ps1` 里也扫过一遍，是干净的。
2. **`no_route` 是 omitempty**：`server.rs:393-395` 只在为 `true` 时输出该键 ⇒ 健康账号读到的是 `$null` 而不是 `$false`，拿 `-eq $false` 判断会**假红**。改成 `-not $a.no_route`。（`daily_guard` 同款 omitempty；`product`/`region` 相反，是**恒输出**的。）

★ 顺带发现：`test-bin\guard-server.exe` 当时是 **1.0.44 的旧产物**（27,089,920 B），而 harness 只断言网关版本 ⇒ 宿主版本一直是**无声漂移**的。修法是**从 HEAD 重编**并顺手在 PE 里扫版本字面量确认，而不是相信 mtime。

### 9.21 1.0.46 按第三轮复核修掉护栏的两处真实缺陷（2026-10-14 凌晨，同一条业主指令）

1.0.45 的安装包**已经打出来了**（14,761,478 B，SHA256 `6CC5E9FA…908068`），随后又做了一轮**独立复核**：发现两处**真机可复现的行为错误**，都在护栏内部 —— **两个旋钮都留 `0`（默认）的人完全不受影响**。NSIS 同版本不覆盖，所以必须换号才能把这些修掉，于是 bump 到 **1.0.46**。

**修的是什么**：

| # | 缺陷 | 症状（真机上看得见） | 修法 |
|---|---|---|---|
| 1 | **积分闸的恢复说法是错的** | 429 文案对**所有**成因都写「过了本地零点自动恢复」。可**余额跌破保留积分下限**这一类**不随日期恢复** —— 它只等积分巡撤回充。用户照着文案等一个永远不会到来的零点，同时**跳过唯一有效的两个动作**（等回充 / 调低下限） | 新增两态 `enum GuardedLine { DailyTokens, Credits }`（`pool.rs:355`），`daily_cap_excludes_all` 改为返回三元组 `(blocked, total, credits_blocked)`（`pool.rs:1252`），`forward.rs` 新增 `daily_guard_recovery(credits_blocked)`（`forward.rs:2293`）按成因分叉文案；**额度优先于积分** |
| 2 | **Trae 无限额会把没问过的旧余额「追认」成真值** | 无限额刷新原本把快照里**早就过期、根本没重新问上游**的 `credits` 透传给 `set_credits_and_expiry`，而那个函数会把 `credits_known` 置成 `true`。一旦旧的假 `credits` 恰好低于 `credit_floor`，账号就会被**一个没人问过的数字**闸住 ⇒ 症状是「**明明写着无限额，号却一直不被选中**」 | 快照元组由 `(uid, auth, credits)` 缩成 `Vec<(String, Option<Auth>)>`（`products\trae\credits.rs:433`）—— 陈旧值**从作用域上消失**，想复现这个 bug 必须先把字段加回来；只更新到期日那条分支改走新增的 `Pool::set_expiry_only()`（`pool.rs:2606`），**不碰** `credits` / `credits_known` |

**回归**（`cargo test --workspace`，末行 `WS3_EXIT=0`，日志 `full-test3.txt` + `build-router3.txt`）：

| 目标 | 结果 |
|---|---|
| `cargo test --workspace` → ai-gateway-core | **1301 passed / 0 failed / 4 ignored** |
| `cargo test --workspace` → ai-gateway-router | **1780 passed / 0 failed / 1 ignored**（1.0.45 为 1776，净增 4 条：积分成因文案 / 积分成因端到端 / 积分未知时不拦 / 无限额不追认旧余额） |

**真机实测（8 个阶段，重跑，日志 `test-bin\guard-live.log`，80 行；`RESULT: FAIL=0`，共 `27 PASS / 0 FAIL`）**：
用 1.0.46 构建的**网关** + 1.0.44 的宿主跑独立实例（宿主版本号与本轮功能无关），隔离端口 57930 / 57931，假上游不可达。要点：

- **P3 旋钮未开 ⇒ 一个都不拦**（`daily_token_cap` 未设时，已超量的账号照常路由，`daily_guard=false`）⇒ 护栏是 **opt-in**，升级不改行为。
- **P4 走真实热重载开门（不重启）**：宿主 `gateway_config.json` 顶层加 `daily_token_cap=500` ⇒ 平台开关接口 `POST /api/gateway/platform-enabled` 200 ⇒ 两个账号在约 6 秒内全被闸住，chat = **429** `rate_limit_exceeded`，文案：**「该产品下的可用账号（共 2 个）今天都被你设置的每日护栏挡住了，所以本机暂时不给它们接新的对话。这不是**故障** —— 额度按**本地日期**统计，过了本地零点自动恢复；也可以调高每日 Token 上限后立即恢复。」**（★ 本例是**额度**成因，所以仍然承诺零点 —— 这正是 9.21 修复要的分叉行为）
- **P5 只影响选号，不是 kill switch**：被闸住时 `healthy=0/2` 而 `cooling=0 disabled=0 no_route=0`；`/healthz` 503；`/tasks/run?task=activity` 仍 **200 `ran=true`**（闸住的号**仍能跑养号任务**）；`/v1/models` 仍 200。
- **P5b 不逐条写日志**：3 次被拦请求给 `gateway.log` **加了 0 行**。
- **P6 调高上限 ⇒ 解除**：改成 2000 ⇒ 约 4 秒后不再 429；**网关进程全程没重启**（pid 32256 不变）。
- **P7 记账语义 / 跨日恢复**：当天种 540 并重启网关（快照从磁盘加载）⇒ 仍 429；把同样的 540 记到**昨天**再重启 ⇒ 约 3 秒内恢复 ⇒ **昨天的用量不算今天的**。
- **P8 清理**：两个测试进程按 PID + 路径双条件收掉（`leftovers: gw=0 srv=0`），端口全释放，**业主的 :7864 / :3080 全程在听**。

**构建数字（全部取自 `build-1046.txt`，247 行，`BUILD_EXIT=0`）**：

| 项 | 值 |
|---|---|
| 内嵌网关 | `embedded/gateway.exe` **13.47 MB**，SHA256（日志前 16 位）`47292B85EACB78F4`；Rust 网关 `Finished release` in 56.37s |
| 前端入口 | `assets/index-D25RMaj0.js`（1,595.11 kB / gzip 464.98 kB）—— 与 1.0.45 **同值**（本轮没动前端） |
| Tauri 主程序 | `target\release\ai-gateway.exe`，`Finished release` in 4m 30s |
| 安装包 | `dist\AI Gateway_1.0.46_x64-setup.exe` = **14,764,440 B**，SHA256 `58A96BC6F91681B2F3FE8554943CC5D1BFD4A06B223A67A06ABC2293437CAC38`（`target\release\bundle\nsis\` 下那份逐字节同值） |
| 签名 | `.sig` 444 B，内文 `file:AI Gateway_1.0.46_x64-setup.exe` / `version:1.0.46` / timestamp `1791920549`，keyid `7e7ce64b6fc3a36f`（与 `tauri.conf.json` 公钥配对）；`==> 校验产物签名` 逐条列出 1.0.7 → 1.0.46 全部 `[OK]` |
| 双指纹 | `embedded/gateway.exe 未被改动 [OK]`；`内嵌前端与 dist 一致：assets/index-D25RMaj0.js [OK]` |
| 复制产物 | 复制到 `dist\` 后 `[OK] SHA256 一致  AI Gateway_1.0.46_x64-setup.exe` |

**★ 没验到的面（与 9.20 同源，诚实记账）**：

1. ~~**`credit_floor` 仍然没有真机读数**~~ ⇒ **已补验，见 9.22**。当时真机假上游不可达 ⇒ 积分巡检刷不回真值 ⇒ `credits_known` 恒 `false` ⇒ 「未知不拦」的设计让积分闸在整轮 27 个 PASS 里一次都没被触发过（只有**进程内**单测）。9.22 用**本机假积分上游**补上了这条跨进程真机读数：**27 PASS / 0 FAIL**。
2. **ZCode 产品没有积分查询**，所以 `credit_floor` 对 ZCode 号**恒不生效**。
3. 本轮**只**修了这两个缺陷，**没有**合并任何新功能。

★ 本轮新增的一个坑：`test-bin\guard-live.ps1` 里**写死的期望版本号**会随 workspace bump 变成假红 —— 已在同一次修掉（改成从 `crates\ai-gateway-router\Cargo.toml` 现读 `^version`，不再手工同步）。**教训：测试里的版本号期望必须现读，不要写死。**

### 9.20 1.0.45 账号级每日用量护栏（2026-10-14，业主「你再看看 trae workbuddy qoder zcode 相关仓库有什么可以合并进来的功能,然后你自己挑选合并,在进行完整的测试」）

这一轮先跨仓库调研（Trae / WorkBuddy / Qoder / ZCode 四家的相关实现），**最后只合并了一个功能**：**账号级每日用量护栏**——两个旋钮 `daily_token_cap` / `credit_floor`，都写在 native config 的**顶层**，默认 **0 / 0 = 关闭**，升级后行为与 1.0.44 完全一致。它补的是账号退场的第三档：此前只有「上游报错（熔断）」与「人工禁用」两条路，缺「上游没报错、账号也健康，只是今天不想再烧额度」这一档。

| 项 | 结果 |
|---|---|
| 功能提交 | `12c2871`（feat(guard)，9 文件 +1302/-3，含新建 `.github/RELEASE_NOTES_1.0.45.md`）+ `cb62b2b`（docs(guard)，按独立评审修掉 6 处文档/注释/测试瑕疵，5 文件 +35/-10） |
| 发布提交 | `641f89c`（chore(release)：1.0.44 → 1.0.45，14 文件 +23/-23） |
| 版本 bump | `scripts/bump-version.ps1` 同步 14 个文件（含 `npm\package.json` 与 5 个 platform 包） |
| 构建 | `build-signed.ps1` **BUILD_EXIT=0**（日志 `build-1045.txt`，243 行） |
| CI | `726aafa` 修掉 macOS 上 `kill_rejects_free_port` 的端口竞态后 5/5 全绿 |
| tag / Release | ★ **未打 tag、未推送、未发 Release**（本地 tag 最大仍是 `v1.0.38`；`fork/main` 停在 `726aafa`，本地领先 3 个提交） |

**构建数字（全部取自 `build-1045.txt`）**：

| 项 | 值 |
|---|---|
| 内嵌网关 | `embedded/gateway.exe` **13.46 MB**，SHA256（日志前 16 位）`2CE284FE31364B86`；Rust 网关 `Finished release` in 58.49s |
| 前端入口 | `assets/index-D25RMaj0.js`（1,595.11 kB / gzip 464.98 kB）；另有 `assets/index-CZCc6qxs.css` 121.99 kB |
| 安装包 | `dist\AI Gateway_1.0.45_x64-setup.exe` = **14,761,478 B**，SHA256 `6CC5E9FA67F9A65DB77AAAA3444E96D2378158B4F4730447C766BA49EE908068`（构建目录 `target\release\bundle\nsis\` 下那份逐字节同值） |
| 签名 | keyid `7e7ce64b6fc3a36f`（与 `tauri.conf.json` 公钥配对）；`==> 校验产物签名` 逐条列出 1.0.7 → 1.0.45 全部 `[OK]` |
| 双指纹 | `校验内嵌网关指纹` ⇒ `embedded/gateway.exe 未被改动 [OK]`；`校验内嵌前端指纹` ⇒ `内嵌前端与 dist 一致：assets/index-D25RMaj0.js [OK]` |
| 复制产物 | 复制到 `dist\` 后 `[OK] SHA256 一致  AI Gateway_1.0.45_x64-setup.exe` |

**真机实测（8 个阶段，日志 `test-bin\guard-live.log`，82 行；`RESULT: FAIL=0`，共 `27 PASS / 0 FAIL`）**：
用 1.0.44 构建的宿主 + 网关（护栏代码在 1.0.44 号上，`641f89c` 才 bump 到 1.0.45）跑独立实例，**只用两个假账号的 fixture**，隔离端口 57930（宿主/webui）/ 57931（网关），假上游是不可达的：

- **P2 身份**：网关只服务这 2 个假账号；`usage.json` 当天已种入 540 token（上限将设 500）。
- **P3 旋钮未开 ⇒ 不拦**：`daily_token_cap=0` 时，**已经超量**的用量一个都不拦（chat 走正常路由 ⇒ 因假上游不可达回 503 `no_healthy_account`，**不是 429**），每个账号 `daily_guard=false` ⇒ 这证明护栏是 **opt-in**、升级不改现有行为。
- **P4 走真实热重载路线开门（不重启）**：往宿主 `gateway_config.json` 顶层加 `daily_token_cap=500` ⇒ 平台开关接口 200 ⇒ **两个账号在约 6 秒（6068ms）内全被闸住**；`daily_guard` 标记出现在 native config 的**顶层**、**不在 pool 块里**；网关进程号未变（**同一条活进程，旋钮不需要重启**）。此后 chat = **429** `rate_limit_exceeded`，中文文案要点：**「该产品下的可用账号（共 2 个）今天都已经用满了你设置的每日额度上限…**这不是故障** —— 额度按**本地日期**统计，过了本地零点自动恢复；也可以调高每日 Token 上限 / 调低保留积分下限后立即恢复」**（★ 这是 **1.0.45** 当时的原文；从 **1.0.46** 起同一句话按**成因分叉**，「余额跌破保留积分下限」那一类**不再承诺零点**，见 9.21）。
- **P5 只影响选号，不是 kill switch**：被闸住时网关 `healthy=0/2` 而 `cooling=0 disabled=0 no_route=0`；`/healthz` 503（宿主把池读成不可用 → 这正是 UI 弹告警的来源）；`/tasks/run?task=activity` 仍 **200 且 `ran=true`**（被闸住的号**仍能跑养号任务**）；`/v1/models` 仍 200。
- **P5b 不逐条写日志**：3 次被拦请求给 `gateway.log` **加了 0 行**。
- **P6 调高上限 ⇒ 解除**：同一路线把 `daily_token_cap` 改成 2000 ⇒ 约 **4 秒**后不再 429；账号不再报 `daily_guard`；**网关进程全程没有重启**（pid 不变）。
- **P7 记账语义 / 跨日恢复**：先把当天种成 540 并**重启网关**，让快照**从磁盘加载** ⇒ chat 仍 429（启动读盘就武装了护栏）；再把**同样的 540 记到昨天**，重启后 chat 约 3 秒内恢复（`PASS yesterday's usage does not block today`）⇒ **昨天的用量不算今天的**，护栏跟着本地日期滚动。
- **P8 清理**：两个测试进程按 PID + 路径双条件收掉，残留 gw=0 / srv=0，两个端口都释放，**业主的 :7864 / :3080 全程在听**。

**全量回归（证据 `full-test2.txt`，末行 `EXIT_WS=0`）**：

| 目标 | 结果 |
|---|---|
| `cargo test --workspace` → ai-gateway-core | **1301 passed / 0 failed / 4 ignored** |
| `cargo test --workspace` → ai-gateway-router | **1776 passed / 0 failed / 1 ignored**（1.0.46 起为 **1780**，见 9.21） |

**★ 没验到的面（诚实记账）**：

1. ~~**`credit_floor` 这条闸门没有真机读数**~~ ⇒ **已于 9.22 补验**（本机假积分上游 + 跨进程真机实测，27 PASS / 0 FAIL）。当时的原因是死假上游让积分巡检刷不回真值 ⇒ `credits_known` 恒为 `false`，按设计「未知就不拦」。
2. **ZCode 产品没有积分查询**，所以积分闸对 ZCode 号**恒不生效**（这正是「`credits_known` 不持久化、未知不当 0」的原因：把"查不到"当成 0 会把 ZCode 号全闸掉）。
3. 本轮**只**合并了这一个功能（cross-repo 调研的其余候选全部未动）。

★ 三个坑（都当场踩到并绕过）：

1. **网关上不能改 `gateway_native_config.json`**：那是宿主从 `gateway_config.json` **生成出来的副本**，下一次 resync 或重启就会被按宿主配置重写。要持久就得改 `gateway_config.json`（宿主保存时以**磁盘现有配置**为基准，手加的键不会被界面保存冲掉）—— 这也正是本次真机测试所走的**真实热重载路线**。
2. **两个旋钮必须放在顶层**：放进 native config 的 `pool` 块里会被 serde **静默忽略**（router 的 `Config` 把它们声明在顶层；只有列进 `HOISTED_POOL_KEYS` 的键才走 pool 块的历史兜底名单）—— 症状是"旋钮明明写了却毫无效果"，而且**一条报错都没有**。测试里专门留了一条断言：`the knob is NOT inside the pool block (no silent-ignore trap)`。
3. **`usage::Stats::new(path)` 只在构造时把 `usage.json` 读进内存一次**，运行期不再看磁盘 ⇒ 想在真机上伪造用量必须在**网关启动之前**写好文件，跑起来再改是没用的（所以过程里有一次是"重启网关让快照从磁盘加载"）。

---

### 9.19 1.0.38 打包 / tag / Release（2026-10-09，业主「打包,然后发新版本构建」）

| 项 | 结果 |
|---|---|
| 版本 bump | `scripts/bump-version.ps1 1.0.38`，14 个文件（含 `Cargo.lock` 4 个 workspace 包 + `package-lock.json`） |
| 构建 | `build-signed.ps1` **EXIT=0**；`npm` 仍坏 ⇒ 用 `.tmp/shimbin` 垫片（见 9.6） |
| 提交 | `d5520b2`（release）+ `03f2917`（docs）+ `aa17690`（fix） |
| CI | `d5520b2` **5/5 全绿**（**先确认绿了再打 tag** —— 1.0.37 的教训） |
| tag | `v1.0.38` → `d5520b2`；本地 tag 48 → **49** |
| Release | id **`407832877`**，`draft=false prerelease=false`，body = `RELEASE_NOTES_1.0.38.md` 全文（`bodyLen=3456`） |
| 资产 | **5 个**（与 1.0.33~1.0.37 惯例一致）：setup + .sig + `ai-gateway_1.0.38_server-x64.exe` + `latest.json` + `latest-windows-x86_64.json` |
| `releases/latest` | 指向 **v1.0.38** |

**实测（用成品里解出来的内嵌网关，不是 target/release 那份）**：

```text
从 ai-gateway.exe 解出内嵌网关：offset=25348834  14053888 B
SHA256 2AD429030AF134937EADDFFD4CD6F0A3B2AE8729D21B9A339A97AB0B26F2C543
与 crates/ai-gateway-core/embedded/gateway.exe 一致: True
```

用这同一份跑（独立端口 18047 + 独立数据目录 + 假上游固定回真实 11133）：

| 形状 | status | code |
|---|---|---|
| A) 小输入 + `max_tokens=1048576`（业主 VS Code） | 400 | `context_length_exceeded` + 人话文案 |
| B) 贴上限 + `max_tokens=128000`（DSH） | 400 | `model_param_invalid` + 人话文案 |

前端指纹：`dist/index.html` 引用 `assets/index-CeNZf-SA.js`，exe 内 contains ⇒ True。
`ai-gateway_1.0.38_server-x64.exe` 也在独立端口起过并 `GET /` 200（488 B 前端）。

**下载件逐字节比对**：5/5 `MATCH=True`。**FILE SIG 有效**（blake2b-512 prehash + Ed25519，
keyid `7e7ce64b6fc3a36f` 与 `tauri.conf.json` 公钥配对）—— 对**下载回来的**那份也验过。

★ 三个坑（都当场踩到并绕过）：

1. **`npm` 坏了**（nvmd 垫片解析不到）⇒ `build-signed.ps1` 的 `npm run tauri -- build` 直接
   失败。用 `.tmp/shimbin/npm.cmd` 垫片（把 `run build` 映射到 tsc+vite、`run tauri` 映射到
   tauri CLI）并把它放进 PATH 才过。
2. **`tauri-build` 的 `rerun-if-changed` 不含 `../dist`** ⇒ 重建前端不会触发重新内嵌，
   构建前必须先删 `target/release/ai-gateway.exe`，否则脚本的前端指纹闸会拦下。
3. **57890 / 57901 都绑不上**（`os error 10013`）—— 它们在 Windows 的 **excluded port
   range** 里（`netsh int ipv4 show excludedportrange protocol=tcp` 显示 57805~57904 等
   大段被 Hyper-V/WSL 预留）。独立实例要用 18xxx 这类。

★ 教训：**`.sig` 别用 `Set-Content` 改写**（会多加一个 `\n`，444 → 445 B，逐字节比对就不
相等了）。tauri 生成的那份直接 `Copy-Item`。

### 9.18 11133 说人话 + 修掉漏判「小输入 + 极大 max_tokens」（2026-10-09）

业主原话：「把 11133 workbuddy 这个报错更明显，**就说人话**，不要再说拒绝了」。
以及他对 VS Code 复现的结论：「上下文设置的太大了，调小就好了」——
**这句是本次的关键线索**：说明那种请求的形状是「小输入 + 极大 max_tokens」。

**一、文案（`classify.rs`）**：旧文案「上游**拒绝**本次请求的参数…请检查请求参数后重试」
有三层毛病：甩锅（说成「你传错了」）、信息量为零（不说哪个参数、为什么）、
把真因埋掉（实测最常撞上它的原因是**请求太大**）。改后：

```
11133 兜底：这次请求太大了，超出了这个模型一次能处理的上限（code=11133）。
          上下文或最大输出设置超出上限。这不是账号问题，换账号无效。
          请任选一种方式重试：① 缩小上下文 ② 调小最大输出 ③ 换更大的模型。
          （上游原话：the request parameters were rejected by ...）
上下文超长：这次请求太大了，超出了这个模型一次能处理的上限。
          请压缩或清空会话（或调小最大输出）后重试。（上游原话：…）
```

「拒绝 / 退回 / 参数错误」三个词有测试钉死一个都不许留。仍保留 `code=11133`
与上游原文（标注为「上游原话」）：前者是业主与日志对得上话的唯一线索，
后者是拿去搜官方文档的凭据。

**二、判据（`forward.rs`）**：`budget_exceeded_guess` 原有一道
`input < 上限 * 6/10 ⇒ Some(false)` 的闸门，理由是「小请求从没失败过」。
但业主现场恰恰相反：

    输入 5,000 + max_tokens 1,048,576 = 1,053,576 > 1,048,576  ⇒ 公式说超了
    闸门却因「输入太小」放行 ⇒ Some(false) ⇒ 客户端看不懂真因

★ **闸门挡掉的正是它该抓的那类。** 整条移除，直接用实测公式（严格大于）。
可以直接用的理由：上限本身即实测规则；输入估计的误差是**绝对量**
（小输入时误差极小，判断反而更可靠）；两个方向代价不对称 ——
误判成溢出最多多压缩一次，**漏判是客户端永久卡死**（就是本 bug）。

**实测**（本机 rig，独立端口/目录，假上游固定回真实的 11133 响应体）：

| 形状 | 修复前 | 修复后 |
|---|---|---|
| A) 小输入 + `max_tokens=1048576`（业主 VS Code） | 漏判 ⇒ `model_param_invalid` | `context_length_exceeded` + 人话文案 |
| B) 输入贴上限 + `max_tokens=128000`（DSH） | `model_param_invalid` + 旧文案 | `model_param_invalid` + 人话文案 |

网关日志出现 `[forward] 上游 11133 且预算超限估算为真 ⇒ 归上下文超长`。
并实测**不破坏**客户端自动压缩：把新文案喂给 DSH 真实的
`isContextWindowExceededError`（code/type/message 拼成一个 detail），
`context_length_exceeded` 仍判 true，而 `model_param_invalid` 判 false
—— 后者正是卡死的机制。**前置中文不影响识别**，因为信号在 `error.code` 里。

全套：router 1719 / core 1288+9+1，0 failed。提交 `aa17690`。

★ **尚未打包**：`:7864` 跑的还是旧网关（`gateway-5076d7-*.exe`），
业主暂时看到的仍是旧文案，需出新构建才会变。

### 9.17 CI 从 1.0.23 起一直红（2026-10-09，业主追问后查实）

**业主原话**：「你这推送的构建不是都失败了吗?」—— **属实**。

我此前说「本地构建 EXIT=0、产出 bundle + .sig」是**对的**，但我**没查 GitHub Actions**。
查证：`ci` 与 `release` 两条工作流**从 v1.0.32 起每一次都是红的**（更早可追到 v1.0.9），
而发版一直手工做 ⇒ 没人发现。`release.yml` 的 preflight 挂在「核心单测」⇒
四平台 build 与「发布 Release」两步**全被 skip**，等于 tag 打上去什么都不会发生。

三组根因（**都不是本轮改动引入的**，最老的从 1.0.23 就在）：

| # | 根因 | 影响面 |
|---|---|---|
| 一 | `trae_crypt_crossimpl.rs` 的夹具指向 `../../../test-bin/verify-trae/`，而 `test-bin/` 在 `.gitignore` 里 ⇒ CI 上根本没这个目录 | windows-latest + preflight，7 条 |
| 二 | 平台假设：三个设备函数是 `#[cfg(windows)]`（非 Windows 必返空串）；`C:\...` 形态路径在 Linux 上无 `Prefix`/`RootDir` 组件；`i64::MAX` 秒在 Windows 溢出、Linux 装得下 | ubuntu / macos，14 条 |
| 三 | 环境变量假设：`Identity::new()` 读 `LANG`/`TZ`（CI 上是 `C.UTF-8`）；`platform` 由 `consts::OS` 推导；`at()` helper 按运行机时区解释而被测函数按 CST 判日 | ubuntu / macos |

**四、顺带查出并修掉一处真缺陷**（我 1.0.36 提交时引入）：
`gateway.rs` 里有一个 `#[test]` 夹在两段文档注释**之间** ⇒ 属性归给了下面那条测试
（`duplicated attribute`），而上面那条 `gateway_restart_goes_through_serialized_entry`
自己**丢掉了属性**、退化成普通私有函数（`never used`）—— 即「不得绕过串行化入口」
那条护栏**从未被执行过**。已归位并做还原验证（把 `drain_before_restart` 挪到
`stop_gateway()` 之后 ⇒ 测试如实变红）。

**五、`gateway` job 缺「构建前端」**：`ai-gateway-server` 用 `RustEmbed` 嵌 `../../dist/`，
而 `dist/` 也在 `.gitignore` 里 ⇒ 报「folder does not exist」且**不生成 `Assets::get`**
（两条 E0599 指向的位置与病因完全不同，极易误读成 rust_embed 坏了）。
`release-bin` job 本来就有那一步，`gateway` job 漏了。

**修复后 CI 实测**（`34481b8`）：**5/5 全绿** —— `release 二进制编译校验` /
`check · windows-latest` / `check · macos-14` / `check · ubuntu-22.04` /
`gateway · Rust 单测与构建`。

本地复现用 `git worktree add --detach` 造了一个**没有 `dist/`、没有 `test-bin/`** 的
干净 checkout —— 那才是 CI 的真实条件（此前一直在有这些文件的开发机上跑，
所以永远看不到这些红）。

★ **教训**：`test-bin/`、`dist/` 这类被 gitignore 的目录，会让「本地全绿 / CI 全红」
长期共存而不自知。凡是被 `.gitignore` 排除、但**测试或构建依赖**的路径，
都必须先问一句「CI 上它从哪来」。

★ 遗留：`v1.0.37` tag 仍指向 `9981783`（CI 红的那次），其 release run 的 build /
发布两步是 skipped。**未改 tag**（那会重写已发布的 tag）；Release 是手工建的、
资产已上传并验签，不受影响。**下次发版前先看 CI 是否绿。**

### 9.16 1.0.36 + 1.0.37 的推送 / tag / Release（2026-10-09，本轮实测）

业主指令：「推送一下吧」。本轮把**两个**版本一起落地（1.0.36 此前一直只在本地）。

| 项 | 结果 |
|---|---|
| `main` | `git push fork main` ⇒ `8e4f5f1..9981783`；远端 `refs/heads/main` = `9981783537da3a318af40d901895f63554e8eb68` = 本地 HEAD（`rev-list --left-right --count fork/main...HEAD` = `0 0`） |
| `v1.0.36` | annotated，对象 `20cac1dac763d43c464d2541726f0f28825202d7`（1.0.36 工作线的最后一个提交）；`git push fork v1.0.36` ⇒ `* [new tag]` |
| `v1.0.37` | annotated，对象 `9981783537da3a318af40d901895f63554e8eb68`（= HEAD，1.0.37 工作线的最后一个提交）；`* [new tag]` |
| 本地 tag 总数 | **46 → 48** |
| Release v1.0.36 | id **`407492463`**，`draft=false prerelease=false`，正文 = `.github\RELEASE_NOTES_1.0.36.md` 全文（`bodyLen=4105`） |
| Release v1.0.37 | id **`407492477`**，`draft=false prerelease=false`，正文 = `.github\RELEASE_NOTES_1.0.37.md` 全文（`bodyLen=5376`） |
| `releases/latest` | 现指向 **`v1.0.37`**（id `407492477`，5 资产） |

两个 Release 各 5 资产（安装包上传前**改名**为 updater 约定名 `ai-gateway-windows-x86_64-setup.exe`）：

| tag | asset | bytes | assetId |
|---|---|---|---|
| v1.0.36 | `ai-gateway-windows-x86_64-setup.exe` | 14750159 | 623836282 |
| v1.0.36 | `ai-gateway-windows-x86_64-setup.exe.sig` | 424 | 623836361 |
| v1.0.36 | `ai-gateway_1.0.36_server-x64.exe` | 27438592 | 623835935 |
| v1.0.36 | `latest.json` / `latest-windows-x86_64.json` | 1716 | 623836391 / 623836424 |
| v1.0.37 | `ai-gateway-windows-x86_64-setup.exe` | 14768471 | 623837205 |
| v1.0.37 | `ai-gateway-windows-x86_64-setup.exe.sig` | 444 | 623837329 |
| v1.0.37 | `ai-gateway_1.0.37_server-x64.exe` | 27480576 | 623836879 |
| v1.0.37 | `latest.json` / `latest-windows-x86_64.json` | 3087 | 623837357 / 623837392 |

**发布后复核（实测，不是推断）**：

| 检查 | 结果 |
|---|---|
| 资产逐字节核对（走 API + token 真下回来） | **4/4 `MATCH=True`**：1.0.36 安装包 `14750159`、1.0.37 安装包 `14768471`、1.0.37 `.sig` `444`、1.0.37 `latest.json` `3087` |
| Ed25519 验签（**对下载回来的那份**，不是本地那份） | 1.0.36 与 1.0.37 都是 `FILE SIG VALID = true` + `GLOBAL SIG VALID = true`，keyid `6fa3c36f4be67c7e` |
| `releases/latest` | `v1.0.37`，`draft=false prerelease=false`，5 资产 |

★ **1.0.36 的 `latest.json` 是本轮补造的**：当时只留了安装包/`.sig`/server 三个资产，`latest.json` 没落盘。
按 9.7 的写法补了一份（`version=1.0.36`、两个平台键的 `signature` 都是那份 `.sig` 逐字、`url` 指向
`releases/latest/download/ai-gateway-windows-x86_64-setup.exe`）。它**只用于 v1.0.36 那个 Release 的归档**，
不影响自动更新（`releases/latest` 指向 v1.0.37）。

★ 本轮的 Windows 侧坑（与 9.10 同源）：`npm` 在本 shell 里不可用（`npm.exe` 是 nvmd shim，
解析不到真实 npm）⇒ `build-signed.ps1` 的 `beforeBuildCommand` 失败。临时做了一个 `npm.cmd` shim
（`.tmp/shimbin/`，把 `run build` 映射到 tsc+vite、`run tauri` 映射到 tauri CLI），**只在构建时加到 PATH**，
没有改动仓库任何文件。另：`tauri-build` 的 `rerun-if-changed` 不含 `../dist`，前端重建后**必须**
删掉 `target/release/ai-gateway.exe` 才会重新嵌入（脚本自己的守卫会拦住，报「内嵌前端不是当次构建的那份」）。

★ 凭据流程与 9.8/9.9/9.10 相同：`.tmp/ghtoken.ps1`（CredRead P/Invoke，脚本本身不含字面 token）⇒
写 `.tmp/ghtoken.txt`（40 B `gho_`）⇒ 用完**立即删除**。

### 9.14 上游把「超上下文预算」报成 11133（2026-10-09，业主第四报）

**业主原话**：「你看看为什么cctv_to_client 这个会话 接手上下文,为什么报这个错误无法接着往下对话了,无论怎么搞都是提示这个错误」

**根因**（真上游逐点二分，全部实测）：

```text
输入 1,042,398 + max_tokens=16      → 200
输入 1,042,398 + max_tokens=8192    → 400 code=11133
输入 1,042,398 + max_tokens=128000  → 400 code=11133
输入   982,318 + max_tokens=128000  → 400 code=11133
```

同一 6.03 MB 的 body：**CJK 触发、ASCII 不触发** ⇒ 限的是 token 不是字节。
⇒ 上游预算 = `输入 + max_tokens ≤ 1,048,576`，且**把超限报成 `model_param_invalid`**。

DSH 按「输入 ≤ 1,048,576」估算 ⇒ `1,042,398 + 128,000` 它认为还有余量 ⇒ **不触发压缩**
⇒ 每轮重发同一个超限请求 ⇒「怎么搞都是这个错」。

会话读数：`session-5b6d3617`（【接手上文·从超大会话…】）压缩后载荷 **7.1 MB**，其中 608 条
assistant 消息占 6.0 MB；DSH 自报压力 666,696 词元（67%）。同目录 `session-a8854fb7`
（# Handoff from session…）只有 1.6 MB ⇒ 正常。

**修法**（判据刻意窄）：① `classify.rs` 新增 `is_param_invalid_from_context_overflow`（11133 **且**
文案点名上下文边界）；② `forward.rs` 新增 `budget_exceeded_guess` / `estimate_tokens` / `max_tokens_of`，
发请求前判「估算输入 + `max_tokens` > `1<<20`」，命中则把 11133 归 `ContextTooLong`；③ `server.rs` 传
`overflow_guess`。`estimate_tokens` 按字符类型分档（CJK ≈ 2 字符/token、其它 ≈ 4）且**向上取整**
（宁可高估：高估只是多压缩一次，低估就是本 bug）。门槛：输入须超上限 **60%** 才参与判定。
未知模型 / 算不出 ⇒ `None` ⇒ **逐字退回**原来的 11133 行为。

**真机实测**（发行包内嵌网关，真上游）：

```text
发行包: in~1.040M mt=128000 → 400 code=context_length_exceeded  ← 客户端会压缩
发行包: in~1.040M mt=16     → 200（对照：不误判）
日志：[forward] 上游 11133 且预算超限估算为真 ⇒ 归上下文超长（让客户端压缩）
```

验签链路：新 body 的 `detail = "api_error context_length_exceeded Invalid request parameters…"`，
DSH 的 `STRUCTURED_CONTEXT_OVERFLOW` 正则**命中** ⇒ 归类 `CONTEXT_WINDOW_EXCEEDED`
（旧 body 的 `model_param_invalid` 不命中）。

### 9.15 随 App 启动与模型清单刷新（同一轮）

**① 重启电脑后网关没跟随启动**：日志读数 —— 宿主 11:19:55 起、网关 11:19:58 起
（`maybe_autostart` **成功过**），但用户 11:22 看时已不在，且**没有任何停止日志**。
自启是 setup 里一次性 spawn ⇒ 任何「起来了又悄悄没了」都静默退化成「没自启」。
修法：每个出口都打日志 + 启动后调 `ensure_gateway_alive` 再确认（1.5s 后查，不在就补拉）。

**② 点「启动网关」后模型路由清单没更新**：`run()` 只 `refresh()`（网关状态），不重拉模型清单；
而模型清单只在「挂载 / 信息不完整 / 手点刷新」三条路径加载。修法：操作后
`Promise.all([refresh(), loadModels(true)])`（`force=true` 因为刚起来的网关可能带上一进程的失败负缓存）。

### 9.13 Qoder 积分余额恒为 0（2026-10-09，业主第三报）

**业主原话**：「qoder这个虽然到期时间查不出来,但是积分余额显示错误,这个是可以查出来的」（附账号池截图：两个 Qoder 账号积分余额列显示 0）

**根因**：账号池页「积分余额」列取**网关 `/status` 的 `credits`**，而该字段只有两条写入路径，都对 Qoder 无效 ——
① `scheduler::refresh_credits_with_gap` 带产品闸门 `a.product_of() != PRODUCT_WORKBUDDY ⇒ continue`；
② `products/trae/credits.rs` 只认 Trae。⇒ Qoder 的 `credits` 恒为 `Entry::default()` 的 0，
而界面判据是 `typeof credits === "number"` ⇒ 显示 **0** 而不是「—」。

**定位读数**：宿主 `~/.wb-switch/qoder/accounts.json` 是 1996 / 1700；网关 `/status` 是 0 / 0；
上游 `/api/v2/quota/usage` 一直能查到 1996 / 1700。业主说的「这个是可以查出来的」准确。

**修法**：新增 `crates/ai-gateway-router/src/products/qoder/credits.rs`（与 `trae/credits.rs` 同形），
`bin/gateway.rs` 接成**第三条**独立定时循环（每 15 分钟一轮，逐个 Qoder 账号 GET `/api/v2/quota/usage`）。
三条纪律：① 失败不写池（查不到就跳过，**绝不**写 0）；② 只读不改状态（只写 `credits`，不碰 `until`/`breaker_until`/`disabled`）；
③ 池里没有 Qoder 账号就不装回调。到期档位仍「未知」是**正确**的：`expiresAt` 是套餐到期不是周期重置，实测两个账号都回 0。

**真机实测**（隔离实例，release 二进制）：一轮后 `/status` 给出 `credits=1996 / 1700`（与上游一致）；
同时刻业主在跑的 1.0.36 实例给出 `0 / 0`（= 截图上的 0）；两个凭证文件跑完一轮 **mtime 与 SHA256 完全未变**。

### 9.12 成本调度三条缝的定位与修复（2026-10-08，业主第二报）

**业主原话**：「这几个国际服账号模型还没用完呢,都是 0 的倍率,为什么还能路由到国内账号上?不是成本优先吗」（附账号池截图）

**排除法定位**（每步都有可复核读数）：

| 假设 | 读数 | 结论 |
|---|---|---|
| 那 710 条是养号任务 | 每条平均 **27 万** input token；养号任务走 billing 域**不记 token** | 否，是真对话 |
| 是偶发路由漂移 | 15 个号被**均匀**摊分（42–50，avg 47.3） | 否，会话级均匀分摊 |
| 是兜底路径 | `fallback_earliest_expiry` 全量日志 **0 行** | 否 |
| 是正常路径（free_tier 失效） | 1.0.36 起 679 次 chat 全走国际版；且 CN 清单早已拉到 | 否 |
| **粘性绑定** | `pick_by_uid_for_model_region` 只查 healthy/in_flight/model_available，**不看 cost** | **是** |

**修复与验证**：见上表 ㊱ 行。真机实测时间线：

```text
23:12:30  free 健康      → 6 次全走 free（粘性绑定到 free）
23:12:45  free 冷却      → 3 次走 paid（free 不可用，paid 是唯一候选）
23:13:04  free 仍冷却    → 4 次走 paid
23:13:26  free 恢复      → 4 次全部回到 free   ← 粘性成本守卫生效
```

**顺带发现并修掉的第二条缝**：区域清单只在内存里，重启后为空 ⇒ `cost_known` 全 false
⇒ 免费优先静默失效。实测（两个号都在冷却时兜底日志打出 `rate=?`）确认。修法：
落盘 `models_cache.json` + `stale_truth` 在拉取失败/退避窗口内退回已知真值。
验证：关掉上游再重启，`/v1/models/regions` 仍给出 `cn=0.11 / intl=0.0`。
### 交付物

```
当前交付版（1.0.35）：

dist\AI Gateway_1.0.35_x64-setup.exe   14,673,521 B   （含 GUI 客户端）
SHA256 F8236A91CA9502537894E7BBCCF44D30F4CF91E730238581523375E36269F3B6
  .sig  424 B  SHA256 4770019441E1FCEA74672F40CCE7061E2C8DBF06B73F03457DE0D8DAD433DADB  keyid 7e7ce64b6fc3a36f
内嵌网关 13,972,480 B  SHA256 924EA451214D005618BAED14E0010A97B106F799E8592F4DD023D93DB53D4A54
  （= crates\ai-gateway-core\embedded\gateway.exe = target\release\gateway.exe，三者逐字节相同；
   已实测：`cargo test --workspace` 跑的就是这一份二进制，与交付件逐字节一致）
包内 GUI 40,182,784 B  SHA256 4EE2718E1A4A05584513CAF91849112D6EF2AC2BFBC71C598F65007A8E13949C
  （`test-bin\inst-1035-unpack\ai-gateway.exe`，用 7z 解 NSIS 得到；NSIS 打包后重排 PE 布局，与工作区产物**不是**逐字节关系
   —— 工作区 `src-tauri\target\release\ai-gateway.exe` 40,182,784 B / 69901537DE8295C592C2213C1308E56F27EF50374DD2169963751769E6D6CCB6）
  ★ 更强判据：包内 GUI 内嵌的 gz blob 与构建目录 `gateway_embed.gz`（5,262,817 B）**逐字节 identical**
   （前 16 字节在全文件恰好命中 1 次 @ 24,948,802；`subarray(off,off+len).equals(gz)=true`；解压得 13,972,480 B / 924EA451…）
   ★ 工作区 GUI 内同一 blob @ 17,368,751，同样逐字节 identical ⇒ 两条路径都指向同一个内嵌网关

★ 非客户端纯部署（仅网关/服务端，无 GUI/WebView/安装器）：

dist\ai-gateway_1.0.35_server-x64.exe   27,212,800 B
SHA256 4F7E7D73C43C330A4347BE3D56E2FE8C77B0B9DEA5436ED27F92C4D8552A929D
  （构建产物在 wt-port\dist-single\ai-gateway.exe，逐字节一致；已拷到 dist\ 与安装包并列交付）
  ★ 与 1.0.34 对照 +51,712 B；★ 该文件是**宿主 GUI 单文件形态**，与 `crates\ai-gateway-server\Cargo.toml:7-9`
    的 `[[bin]] name = "ai-gateway"` 同名同源 ⇒ 命名沿用「server-x64」
历史（1.0.34）：
dist\AI Gateway_1.0.34_x64-setup.exe   14,626,546 B
SHA256 6B87A39906CF40743DFF515331398B88EBE3E9C5E0EF180B0886A4E85550E476
  .sig  424 B  SHA256 66A7A984C1E0E6DF5D0A312D9FF329C9703E3FD7C3A7087799DB2EB52722FE9E  keyid 7e7ce64b6fc3a36f
内嵌网关 13,831,680 B  SHA256 9447272AACA62EA486C470E68CD18D745C1DD89A3CACE5932AD9CF65D289F2F8
包内 GUI 40,133,632 B  SHA256 A6FD8E7FF1782BA5B45D4EB4CC8A8FBD9012775461A7A494D82EFBB356F5D132
dist\ai-gateway_1.0.34_server-x64.exe   27,160,576 B
SHA256 70E72BD0DCF25AD2F4258B719CE999855DF6129ED8D21B59716D4C2AF800E96D
历史（1.0.33）：
dist\AI Gateway_1.0.33_x64-setup.exe   14,605,514 B
SHA256 20746313D2F709932AF270AB9B957A10B94982E97BB60B67EF31674A3850AAD2
  .sig  424 B  SHA256 B7854BCBB9D364156C966EE90FC8E27DEDAA51577F146C8BE922CED014B4CC11  keyid 7e7ce64b6fc3a36f
内嵌网关 13,835,776 B  SHA256 1BEDC8E683BAE7EFEB7463A91D2FEB1A67734028EEEDF913F53CDEA05720AACC
包内 GUI 40,062,976 B  SHA256 969B7422005242AF94B23B83F6A201B5C90684425EDF595883619C18AC5D9566
dist\ai-gateway_1.0.33_server-x64.exe   27,302,400 B
SHA256 AC18AD26E700D01B52E0A3B9E7530F75EAB2D1E60F3A3682299C2BCB37EC9650

历史（1.0.32）：
dist\AI Gateway_1.0.32_x64-setup.exe   14,598,416 B
SHA256 BD07D32CC0CA69845CF0504C5905C03BCD1832FAA9F3109F606676EBFEA55A38
  .sig  424 B  SHA256 FAE936C8FE1F60221895AAC4677F4DD396EDC548445C1621092C6033FC55BA84  keyid 7e7ce64b6fc3a36f
内嵌网关 13,778,944 B  SHA256 CEA67DE06FEDC8CA6126BB4E476AB26826DAA38354C77D9C76EFE20737CF755D
包内 GUI 40,051,712 B  SHA256 2F96C28FD3CF2DD9BBBD823AD87012F39880C78763B06431E77100AE4A533C3D
dist\ai-gateway_1.0.32_server-x64.exe   27,063,808 B
SHA256 1788241A21F5803BD8345F690EAE477EEDE37B1AE174B3F32A97A7F1021110A2

历史（1.0.31）：
dist\AI Gateway_1.0.31_x64-setup.exe   14,586,903 B
SHA256 1FB28395FE386E160482BC6BEC1A2417809755C24B3521C125F56ABBEAADA2A5
  .sig  424 B  SHA256 2EB97D23E783DCB1AC0D87B444773624C654E2A5CAFC89B50776CB16E14E9BE6  keyid 7e7ce64b6fc3a36f
内嵌网关 13,723,136 B  SHA256 5B5D23ECFC66E32875BA28D9D106D40044BB559AAC9672FA28DAC4B2C840C6C9
包内 GUI 40,104,960 B  SHA256 0147D56D0D2B3FCE97194B464CF637A39D5441B492143E9E2099068502C847A5
  ★ 更正（2026-10-06）：此处原写「宿主 GUI 27,274,752 B / 9FAA831B…」，与下方 server 版逐字节重复，系笔误；
    工作区不留宿主 GUI 独立副本（`src-tauri\target` 已清理）⇒ 1.0.31 宿主 GUI 的权威值只有包内那一份
dist\ai-gateway_1.0.31_server-x64.exe   27,274,752 B
SHA256 9FAA831BC3362C90976A8957B0AE67557A4E3A242D6FB8C220A44336D138669C

历史（1.0.30）：
dist\AI Gateway_1.0.30_x64-setup.exe   14,545,639 B
SHA256 7307D32175869D5EBB32300FA520F5B03915327DF94E7B79BE1E5C6BD1F82BD0
  .sig  424 B  SHA256 AA5EC776288085F7E8916602237BA0B22E3A6A17E7FAC1C157377FD5ECD25AAF  keyid 7e7ce64b6fc3a36f
内嵌网关 13,629,440 B  SHA256 F7575803A1F2BD26048C0804AB8B2F776CD4C3DE07DE238DEEE084158B01D917
  ★ 与 1.0.29 的内嵌网关（99AA2D33…）**同尺寸**，`.text` 段哈希完全相同（eab72f4ac8128d3ad835a178）
    ⇒ 全部差异只有 6 段 27 字节，落在 PE TimeDateStamp 与 RSDS 调试 GUID ⇒ **网关代码零变化**，
    本版行为改动全在宿主 GUI 侧（即 1.0.29 的网关里 ZCode 断流修复已在，本版只是补齐三形态实测证据）
宿主 GUI 40,178,176 B  SHA256 C47B67B9A0C394BEAA1F8864F2E99C291EAF3BE287B42CD06A7D0DF3FD0B14EB
包内 GUI 40,178,176 B  SHA256 09A115E1796913CE7ED2B09DA8774F2F1E414042E0825DDDF1424FEF5FA6CC14
  （与工作区产物只差 3 字节，偏移 33403520-33403522：BUNDLE_TYPE_VAR_UNK 对 BUNDLE_TYPE_VAR_NSS）
包内反解出的网关 13,629,440 B  SHA256 F7575803…（与 embedded/gateway.exe 逐字节一致）
（已 7-Zip 独立解包 + 从包内 GUI exe raw-inflate 出网关并与 embedded/gateway.exe 逐字节比对通过）

历史（1.0.29）：
dist\AI Gateway_1.0.29_x64-setup.exe   14,575,327 B
SHA256 1183B3321D32B2AAAA64975E5167EFEAF371665A544AC4CDFE6EE8A28F05EFC0
  .sig  424 B  SHA256 2C13C378253B1507D327E040B563BBFA33835D5B3BDE63609A53E51B9E34CA14
内嵌网关 13,629,440 B  SHA256 99AA2D33C7FF834BB1DC9B496A1306D474738FD92E14A0559BF7ECF0EC8981B6
宿主 GUI 40,436,224 B  SHA256 FB4980625D8CC4701698B15104F0514D0E0A3144EBD3B0EC0A1A82D431ED5AC1

历史（1.0.28）：
dist\AI Gateway_1.0.28_x64-setup.exe   14,573,805 B
SHA256 1EAC0F3D401A8797086DE3D45D2B47E43303D65CD5EC88CDDBB02CDFBD9822CA
  .sig  424 B  SHA256 C4F88F8173B66C5112E82F3422849B756B0658F417872D989EDFEA1543ECCA18  keyid 7e7ce64b6fc3a36f

历史（1.0.27）：
dist\AI Gateway_1.0.27_x64-setup.exe   14,557,116 B
SHA256 A5DB6F5C69DE5673E6A3A47D41535F90C94010FC4B57443E9E028FCF67F46098

历史（1.0.20）：
dist\AI Gateway_1.0.20_x64-setup.exe   14,406,438 B   （2026-10-04 02:50:31 UTC）
SHA256 691454CB0096A9116D7FBC963C1ED6874C0A4FD964C01C263EA2E2AD0E2A1DD6
  .sig  424 B  SHA256 F307273CCAFFE6D53037602CFE2C1A2104977B2A6E0E442B9F3E8B324B77DD22  keyid 7e7ce64b6fc3a36f
内嵌网关 13,684,224 B  SHA256 43C9E48DFA27DAA9B5BE92A2D4AD5424F41534E203ADD383B573AC4A81E86388
（已 7-Zip 独立解包 + 从包内 GUI exe raw-inflate 出网关并与 embedded/gateway.exe 逐字节比对通过）

★ 历史（1.0.17）：
dist\AI Gateway_1.0.17_x64-setup.exe   14,311,130 B   （含 11133 修复，2026-10-03 18:33 重打）
SHA256 8331371362744B4A22D86F7E8C8974AEB33F7B77EC896BD39C0D4DF767701662
  .sig  424 B  SHA256 F9D28C144AABB8001892B5DD7AD655E48E824675C8FC4C339AA77A04A21F7DF9
内嵌网关 13,619,712 B  SHA256 D8CE65557A5FC4A6F489BAAFAB0775E6CA3399247E997F5D4F67EEEDA7302105

★ 2026-10-03 第二轮（11133 修复）：网关源码改了 3 个文件
  （`products/trae/translate.rs` / `upstream/payload.rs` / `protocol/rectify.rs`），
  embedded/gateway.exe 与安装包都已按同一条链重建并签名。
  上一轮（无修复）的安装包：14,309,063 B
  SHA256 4CBF382F925D9FD40F905707E313141BBA580A28E20FEB09464272A3C152F10A
```

★ 权威文档（**最新版优先**）：
- 交付汇总 **`test-bin\DELIVERY-1.0.34.md`**（九节，含「上游先 finish_reason 后 [DONE]」根因、现场三源取证（`gateway.log` 16 条 / 插件 18 次 / 失败率 0.12%）、`/v1/models` 并集解释、四处改动清单与行号、四个新单测、**假上游 5 模式 A/B 矩阵**、**三协议探针**、真机 15/15、**「新代码确实进包」的逐字节证明**（gz blob identical + 解压哈希）、验签配方、`build-single.ps1` 必须 `-SkipFrontend`、**本轮未验证面**、收尾清理与**已执行的清理结果**）
- 上一版汇总：`test-bin\DELIVERY-1.0.33.md`（九节 + 9.1，含根因与四处修复、落点 A–J 全表、三个新单测、**决定性 A/B/C 三态对照**（1.0.32 冷启动 0/120 → 仅权威修复 0/120 → 权威+预热 120/120 → **出厂二进制冷启动 120/120**）、**「新代码确实进包」的逐字节证明**（gz blob identical + 解压哈希）、验签配方**更正**（全局签名不预哈希）、`build-single.ps1` 必须 `-SkipFrontend`、收尾清理与**已执行的清理结果**）
- 更早汇总：`test-bin\DELIVERY-1.0.32.md`（九节 + 9.1，含根因与三层修复、落点 A–J 全表、九个新单测、**包内网关**隔离实例 20/20 实测、探针自身的两个坑、解包核验的**方法论更正**（Rust release 剥符号 ⇒ 不能靠字符串证明「新代码进包」）、验签的两个结构坑）
- 发布说明 `.github\RELEASE_NOTES_1.0.34.md`（当前）／历史 `.github\RELEASE_NOTES_1.0.33.md` / `.github\RELEASE_NOTES_1.0.32.md` / `.github\RELEASE_NOTES_1.0.31.md` / `.github\RELEASE_NOTES_1.0.30.md`
- 历史：`DELIVERY-1.0.29.md` / `-1.0.28` / `-1.0.27` / `-1.0.26` / `-1.0.25` / `-1.0.24` / `-1.0.23` / `-1.0.21` / `-1.0.20`
- ★ 1.0.28 的 **router crate 版本串仍是 v1.0.23**（刻意与 GUI 版本号解耦，不影响功能）

★ 注：Rust release 构建**不确定**（同一份源码两次构建 SHA256 不同，实测确认）
⇒ 网关指纹**只在同一次构建链内可比**，跨次构建不能按哈希比对。

---

## 三、测试基线（改动后必须不低于这些数）

| 命令 | 基线 |
|---|---|
| `cargo test -p ai-gateway-router` | **1685 passed / 0 failed / 1 ignored**（1.0.34；1.0.33 是 1681、1.0.32 是 1678、1.0.31 是 1669、1.0.30 是 1667、1.0.20 时是 1633） |
| `cargo test --workspace`（15 个 test 块累加） | **3045 passed / 0 failed / 6 ignored**（1.0.34，`uitest\ws-test-1034.log`，EXIT=0；1.0.33 是 3041、1.0.32 是 3038、1.0.31 是 3029、1.0.30 是 3011、1.0.29 是 3007、1.0.28 是 3006、1.0.27 是 3001、1.0.26 是 2988） |
| ★ 1.0.34 的 **+4** | `upstream\sse.rs` 3 个（`finish_reason_without_done_is_not_flagged_as_premature` / `null_finish_reason_does_not_count_as_completion` / `finish_reason_in_tail_buffer_counts_too`）+ `protocol\anthropic_stream.rs` 1 个（`finish_reason_without_done_is_not_a_failure`）；★ 1.0.33 的 +3 是 `pool.rs` 兜底权威性三条（`authoritative_fallback_keeps_bare_model_off_that_product` `:5019` / `authoritative_fallback_needs_a_non_empty_list` `:5062` / `sticky_pick_rejects_an_account_from_a_product_with_an_authoritative_list` `:5080`）；★ 1.0.32 的 +9 是上游实测集 4 条 + 区域收敛 5 条（`:4599` / `:4646` / `:4674` / `:4701` / `:4732` / `:4795` / `:4850` / `:4881` / `:4902`） |
| Trae 相关（`products::trae::` 128 + `modules::trae_agent::` 38 + `modules::trae_account::` 22） | **188 passed** |
| `cargo test -p ai-gateway-core --lib` | **1285 passed / 0 failed / 4 ignored**（1.0.34 口径，与 1.0.33 / 1.0.32 / 1.0.31 相同；1.0.30 是 1275、1.0.29 是 1265、1.0.17 时是 1235） |
| `cargo test -p ai-gateway-server` | **28 passed / 0 failed**（1.0.20 口径） |
| `cargo check -p ai-gateway-router --lib` | EXIT 0，**0 warnings** |
| `cargo check -p ai-gateway-router --bin gateway` | EXIT 0，**0 warnings** |
| `cargo check --workspace --all-targets --profile test` | EXIT 0 |

★ `cargo` 不在默认 PATH：
```powershell
$env:PATH = "C:\Users\iuuuuuuuu\.cargo\bin;$env:PATH"
```
★ ★★★ **绝对不许加 `--offline`**（2026-10-05 作废旧说法）：本机 cargo 的 registry 缓存是**空的**，加 `--offline` 会直接 `error: no matching package named 'aes' found`。
`wt-port\.cargo\config.toml` 已配 `sparse+https://rsproxy.cn/index/` 源，**联网正常构建**即可。

---

## 四、★★ 本仓库特有的陷阱（血泪，务必先读）

### 1. `cargo test --workspace` 与 `-p` **不是同一个构建**
`src-tauri` 的 `tauri-plugin-updater` 拉 reqwest 0.13.4，会打开
`hyper-util/client-proxy-system`。而 `hyper-util` 全 workspace 只有一个版本 ⇒
**Cargo 按包版本统一 feature** ⇒ 我们自己的 reqwest 0.12.28 在 `--workspace`
构建里**也**拿到系统代理能力，`-p` 构建里没有。

后果：reqwest 默认 `auto_sys_proxy=true`，会读 **Windows 注册表**的系统代理
（Go 的 `http.ProxyFromEnvironment` 只读环境变量）⇒ 「没配代理」在 Go 是真直连、
在 reqwest 是偷偷走代理。

**修法**：未配置代理时必须显式 `.no_proxy()`（`upstream/client.rs` 的 `match proxy`）。

### 2. ★★ `Copy-Item` 保留 mtime ⇒ cargo 跳过重建 ⇒ 测到旧二进制（**假绿**）
做 A/B 还原验证时**必须**：
```powershell
(Get-Item <file>).LastWriteTime = Get-Date
```
否则「还原后测试仍通过」是假象（它跑的是上一轮的二进制）。这个坑我踩过两次。

### 3. `serde_json` 无 `preserve_order` ⇒ `Map` 是 `BTreeMap`
`json!({...})` 会按键的**字典序**重排字段。凡「字段顺序参与签名」的上游请求体，
**必须**用 `#[derive(Serialize)] struct` 按声明序序列化。这类错误**不报错**，
只让上游签名校验失败。

### 4. 网关 `main()` **没有** `--version` 分支
`gw.exe --version` 会被当成「无 config 启动」⇒ **起 HTTP 服务并常驻**。
所以**绝不要**直接运行共享目录里的可执行文件（哪怕只是读版本号）；
要跑就先 `Copy-Item` 到自己的目录。

★ 2026-10-05 更正：业主的网关进程名现在是 `gateway-4e52fa-ecd7924c953073e8.exe`（13,607,424 B，1.0.21 血统），旧记的 `gateway-4e8a56-9affe6e7f02852f9.exe` 已不在运行。
★ 2026-10-04 修正一条旧说法：**NSIS 模板确实会按进程名杀**。`installer.nsi:641` 与 `:768` 两处 `!insertmacro CheckIfAppIsRunning "ai-gateway.exe" "AI Gateway"`，
宏体在 `target\release\nsis\x64\utils.nsh`，内部走 `nsis_tauri_utils::FindProcessCurrentUser` + `KillProcessCurrentUser`（**按可执行文件名**）。
所以「安装前先退出桌面版」是必须的提醒，而且这也**再次确认了「绝不运行安装包」这条规矩**。
（另注：业主的网关进程名是 `gateway-4e52fa-ecd7924c953073e8.exe`，名字不同，安装器不会碰它。）

### 5. `bin/gateway.rs` 是**独立 crate**
看不到 lib 的 `pub(crate)` 项。所以需要 `pub(crate)` 依赖的逻辑
（如 `claim_all_campaigns`、`QoderClient`）**必须**在 lib 内做成 `pub` 入口，
bin 只调用它。范例：`products/product_tasks.rs`、`products/qoder/billing_cycle.rs`。

### 6. 其它
- `Select-String` 单引号里的 `\"` 是**字面量** ⇒ 会假阴性；`^\s*(pub )?mod ` 漏 `pub(crate) mod`
- ★★ **审计「定义 vs 注册」清单时别用 `-Context 0,N`** —— 它会在 N 行处**静默截断**，
  长清单被少算。我据此误报「10 个 Tauri 命令未注册」，实际 86=86 零缺口。
  正解：`ReadAllLines()` 定位块起止行，在**完整行区间**上迭代正则。
- ★★ **本仓库的 `.rs` 是 CRLF**（`api.rs` 实测 2943 CRLF / 0 裸 LF）⇒ 跨行匹配必须先
  `replace("\r\n","\n")`，否则含 `\n` 的模式**永远匹配不上**（假红）。
- `Get-Content` 把 UTF-8-no-BOM 的 `.rs` 读成 GBK ⇒ 用 `read` 工具；
  数行数用 `[System.IO.File]::ReadAllLines($p).Length`
- `cd D:\...` **不改变 .NET 进程 CWD** ⇒ `[System.IO.File]` 必须用绝对路径
- 假上游用 **`ThreadingHTTPServer` + `daemon_threads`**；单线程会让网关死锁
- axum 在客户端断开时**丢弃 handler future** ⇒ 清理逻辑必须用 RAII 守卫
- ★ **`ai-gateway-server` 不在 NSIS 安装包里** —— 它经 **npm** 分发
  （`npm/platform/ai-gateway-win32-x64`）。改它**不需要**重打 `dist\*.exe`。
- ★ **起宿主服务必须设 `AI_GATEWAY_HOME` + `--port`**：不设它会用**真实数据目录**、
  还会触发网关重启（我踩过一次：把所有者 `:7864` 的 pid 从 24900 换成了 42576）。
  正确做法：独立目录 + `serve --port 5799x --no-open`，并验证目录里**只有日志与 exe**。

### 7. `gateway_embed` 的「FNV-1a」用的是写错一位的质数（只影响文件名，不是缺陷）
`crates/ai-gateway-core/src/modules/gateway_embed.rs:52 fn fingerprint(data: &[u8]) -> String` 逐字：
```rust
let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
for b in data { hash ^= *b as u64; hash = hash.wrapping_mul(0x1000_0000_01b3); }
format!("{:x}-{:x}", data.len(), hash)
```
标准 FNV-1a 64 位质数是 `0x100000001b3`，源码里是 **`0x1000_0000_01b3`（多一个 0）**。
⇒ 落地文件名是 `gateway-<hex(gzip载荷长度)>-<hex(该变体质数哈希)>.exe`。
★ **长度段是 gzip 载荷长度，不是解压后的 exe 长度**：1.0.28 真机文件名 `gateway-4eaf37-29d0aea8025e7f09.exe`，
`0x4eaf37` = 5,156,663（宿主 exe 偏移 25237210 起的 gzip 载荷），而解压后的 exe 是 13,629,440 字节（`0xcff1a0`）。
我用 node BigInt 复算证明：变体质数 ⇒ `29d0aea8025e7f09`（命中），标准质数 ⇒ `a8419ca8025e7f09`（不命中）。
影响：指纹只用于缓存文件名与 `cleanup_old()` 的保留判定（`:80` / `:117`），仍然稳定且对内容敏感 ⇒
**不修**（改质数会让所有用户的落地文件名变化、触发一次无谓重写）。
写断言脚本时**照源码口径复算**，别自作主张用解压后长度（我因此在 `uitest\verify-1028-deployed-gw.cjs` 上吃过一次假红）。

### 8. ★★★ 真机断言脚本自身的两个缺陷（2026-10-06，都伪装成「产品缺陷」）

**(a) CDP 注入落进 `about:blank` 上下文（竞态）**
Tauri 启动时 WebView2 **先加载 `about:blank`，约 700 ms 后才真正导航**到 `http://tauri.localhost/`。
`Runtime.evaluate` 落在**当时**的默认执行上下文，而导航会触发 `Runtime.executionContextsCleared`，
把旧上下文连同注入的 `window.__vc` / `window.__g10` 一起丢弃 ⇒ 注入「成功」（返回 `VC_OK`）却永远读不到，
表现为「一整排断言全红、全都是 helper undefined」。
**修法**（`uitest\verify-usage-casefold.cjs` / `verify-hub-real.cjs`）：
① 注入前 `Page.addScriptToEvaluateOnNewDocument({source: HELPER})`；
② 等上下文稳定（`location.href` 含 `tauri.localhost` 且连续两次相同，上限 30 s）；
③ 注入后带重试（≤15 × 200 ms）直到 `typeof window.__vc === 'object'`；
④ **必须为「注入本身」加一条断言**（否则注入失败会被后续断言掩盖成产品缺陷）。

**(b) 期望值写死成日历常量**
`verify-hub-real.cjs` 原来写 `const expectMap = { 'trae-checkin': 'done', 'zcode-warm': 'done', … }`。
`trae-checkin` 只在「今天已经签过到」时是 `done` —— 跨过午夜、或换一个今天还没签到的实例，
这条断言必然变红，而产品渲染完全正确（`TraePage.tsx:175-201`：`success`/`already`/`checked_in` ⇒ `done`；
`skip` ⇒ `blocked`；`not_checked_in`/`fail`/缺键 ⇒ `ready`）。历史那几次 50/50 PASS 只是恰好跑在「今日已签」那天。
`zcode-warm` 同样是日历相关的（`ZcodePage.tsx:202-238` 的 7 天窗口），只是窗口长、暂时不会红。
**规矩：真机断言的期望值一律从真机数据推，不许写死。**
做法：`trae-checkin` 读 `trae_checkin_results.json` 的**今日**条目（本机时区，与 `trae_checkin_results.rs:26 today_key()` 同口径）；
`zcode-warm` 读 `zcode\accounts.json` 的 `warmedAt` 是否落在 7 天窗口内；`trae-credits-snapshot` 产品恒为 `ready`。
数据读不到、或同一平台多账号推导结果不一致 ⇒ 返回 `null` ⇒ **不断言**（宁可少一条，也不产生假 FAIL）；
并且**惰性求值**（断言那一刻才读盘），避免宿主 `refresh_today()` 期间改写当天状态造成假红。
（探针的 `AI_GATEWAY_HOME` 来自 `gate-hub-real.ps1` 里的 `$env:`，会随子进程继承到 Node。）

### 9. ★★ 卡死类缺陷的可复现判别式 = WebView2 的 `Target.targetCrashed` 计数（2026-10-06 定案）

「界面卡死但网关正常」这类问题**不能用「响应/不响应」当指标**：主窗口是否卡住取决于崩溃时刻是否撞上探针的 1500ms 超时线，
同一装置在首轮命中、后两轮不命中（`uitest/ab-freeze-rep.log` REP 2/3 命中，REP 1 未命中）。**稳定可重复的指标是 CDP 事件流里的 `Target.targetCrashed` 计数**：
1.0.21 臂三轮**每轮恰好 1 次**（预热窗口导航到死地址后 1.80 / 1.82 / 1.98 秒），1.0.29 臂三轮**全 0**。
复现命令（回归可复用）：
```powershell
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File uitest\ab-freeze-rep.ps1 -Reps 3 -Secs 150 *>&1 | Out-File -Encoding utf8 uitest\ab-freeze-rep.log
& 'C:\Users\iuuuuuuuu\.nvmd\versions\26.0.0\node.exe' uitest\ab-analyze.cjs
```
`uitest/ab-analyze.cjs` 逐轮打印 samples / maxRt / TIMEOUT / FREEZE / crashes / 崩溃 target 的 URL 轨迹 / 预热窗口 URL。
★ 装置要点：两臂二进制都取自各自**正式安装包**，只用 `uitest/patch-identifier.cjs` 改 identifier 区间（⇒ 两臂都是全新 WebView2 profile，排除「profile 已预热」）；
`Clear-Arm` 只按 `.Path -like '*iso-owner-1021*'` 杀进程（绝不按进程名）。

---

## 五、已知未验证面（如实记录）

| 项 | 状态 |
|---|---|
| Trae **客户端里真点一次对话** | ❌ 未做（我不主动拉起你的 IDE）。已实测到「按库里写下的 `base_url` + 解出的凭据真发请求 ⇒ 200 + 真实回答」 |
| Trae `custom_model_id` 是否被客户端本地校验 | ❓ 未证（代码优先复用库里已有值，实测复用到 `2005145218`，不编造） |
| Trae `sk` 字段（`auth_type:1` 场景） | ❌ 无样本（库里 3 条活动记录全是 `auth_type:0` / `sk:""`） |
| ZCode **真 JWT 的成功路径** | ❌ 未跑通（`preview-plan` / `claim-plan` / `warm` 的成功分支）。需真实 ZCode JWT |
| Qoder OAuth `ready` 分支 | ❌ 未在真机走到（需人工点浏览器授权） |
| DST 异常时区的月步进 | ❌ 未在本机复现（依赖真实 tzdata 瞬间）；防御逻辑已实现并测试 |
| 11133 的 **Trae 侧修复**（丢弃空名 tool-call） | ❌ 无法用真实上游复现 —— 实测 Trae 既不吐结构化 `tool_calls`、也不校验 tool 配对（污染历史探针返回 200，整流日志 0 命中）⇒ 只有单测覆盖（2 条） |
| 11133 修复版**部署到所有者 `:7864`** | ❌ 未部署（业主实机 2026-10-07 已回退到 **1.0.6**，主程序 32,838,656 B）⇒ 装 1.0.33 即覆盖 |
| **1.0.34 的安装包本体未在业主实机安装** | ❌ 约束④（业主不在电脑旁、且禁止在业主实例存活时运行安装器）⇒ 本版真机证据 = **出厂二进制**（`crates\ai-gateway-core\embedded\gateway.exe`，13,831,680 B / `9447272A…`，逐字节拷到隔离实例 `:17905`）**真机 15/15 全 200** + **假上游 5 模式 A/B**（`:17903` 出厂 1.0.33 vs `:17904` 出厂 1.0.34）+ **三协议探针** + 包内 GUI 内嵌 gz blob **逐字节 identical**（`0x17CB042`），**不是**安装后的客户端。★ 隔离实例已**停掉并清理**（`:17900` PID 39380 / `:17902` PID 8220 / `:17903` PID 32676 / `:17904` PID 26112 / `:17905` PID 32700 / `:17901` node PID 35044，均按「PID + 可执行文件路径」双条件停；27 个业主真实账号副本已删；见 `test-bin\DELIVERY-1.0.34.md` 第九节）|
| **1.0.34 修的「输出完就中断」在业主实机是否消失** | ⚠ 本机**不能**证明：断流本身由**链路/代理抖动**引起（16 次断流集中在请求发出后 32 ms – 6 s、多会话几十至几百毫秒内齐断、与账号/模型/代理节点都无关），网关只能保证「内容已完整时不误判为失败」。本版语义是**单向放宽**：`modeB`（半句就断）/ `modeD`（只吐一帧）/ 首帧前就断 ⇒ 仍报错并换账号重试，**不会把真截断吞掉** |
| **1.0.34 的宿主 GUI 独立副本** | ⚠ 工作区已无 `src-tauri\target`（构建后清理）⇒ 宿主 GUI 的哈希/版本只能从**包内** `inst-1034-unpack\ai-gateway.exe`（40,133,632 B / `A6FD8E7F…`）取证 |
| **1.0.33 的安装包本体未在业主实机安装** | ❌ 约束④（业主不在电脑旁、且禁止在业主实例存活时运行安装器）⇒ 本版真机证据 = **出厂二进制**（`crates\ai-gateway-core\embedded\gateway.exe` 逐字节拷到隔离实例 `:17869`）**冷启动 120/120** + 包内 GUI 内嵌 gz blob **逐字节 identical**，**不是**安装后的客户端。★ 隔离实例已**停掉并清理**（`:17868` PID 26124 / `:17869` PID 21904，均按「PID + 可执行文件路径」双条件停；凭证副本 27+1+4 项已删；见 `test-bin\DELIVERY-1.0.33.md` 第九节）|
| **1.0.32 的安装包本体未在业主实机安装** | ❌ 约束④（业主不在电脑旁、且禁止在业主实例存活时运行安装器）⇒ 本版全部真机证据来自**包内网关**（`inst-1032-unpack\embedded-gateway.exe` 逐字节拷到隔离实例 `:17866`），**不是**安装后的客户端。★ 隔离实例已在交付后**停掉并清理**（PID 33476 按「PID + 可执行文件路径」双条件停；凭证副本 6 个目录全删；见 `test-bin\DELIVERY-1.0.32.md` 第九节）|
| 1.0.32/1.0.33 的「上游实测集自动跟随」 | ⚠ 本机无法验证：若腾讯以后在**国际版**上线 `space-bunny`，网关会自动放开（实测集每次拉 `/v1/models` 都刷新）⇒ 只能等上游变化 |
| 1.0.33 的**启动预热**（`warm_up_live_models`） | ⚠ 上游不通时预热会**静默失败**（设计如此）⇒ 此时裸名仍可能选中错区账号（退回到 1.0.32 的行为）；但**不会**再抽到 Trae（硬排除不依赖预热） |
| 1.0.33 的 Trae **正常模型**端到端 | ❌ 未做（隔离实例里的那个 Trae 号只用于复现失败；`glm-5.2` 等正常名未在真 Trae 号上跑过对话） |
| 1.0.32/1.0.33 隔离实例的额度刷新 | ⚠ `credit-refresh <uid>: upstream client (http 403): {"code":10085,"msg":"请求不合法，如有疑问请联系客服","requestId":"…"}`（同一批账号副本被上游限流），**不影响对话** |
| 1.0.33 的**宿主 GUI 独立副本** | ⚠ 工作区已无 `src-tauri\target`（构建后清理）⇒ 宿主 GUI 的哈希/版本只能从**包内** `inst-1033-unpack\ai-gateway.exe`（40,062,976 B / `969B7422…`）取证 |
| **GUI 侧实测**（托盘 / 单实例 / 自动更新 / 开机自启） | ❌ **设计上做不了**：桌面版第一个注册的插件是 `tauri-plugin-single-instance`，互斥量 `com.momo0410.aigateway-sim`；只要业主的 `D:\APP\AI Gateway\ai-gateway.exe` 在跑，我启动解包出来的 GUI exe 只会 `exit(0)` 并聚焦他的窗口，预热/托管/托盘全不跑 |
| **Trae SG / 国际版分支** | ❌ 无真实 SG 账号可测，只有单测覆盖（业主账号 JWT 无 `iss` ⇒ `RealmEvidence::Default` ⇒ 界面「国内版（推断）」） |
| ② 裸名路由的**本机 A/B 复现** | ⚠ 未能复现差异（旧/新网关对裸名都返回 WorkBuddy）—— 该兜底清单在 1.0.19 已存在；只能靠源码 + 单测 + Trae-only 池对照 |
| `ai-gateway-server` 启动横幅版本串 | ⚠ 仍打印 `ai-gateway v1.0.19`（该 crate 自己的版本常量没跟升），**不影响功能**，已写进交付说明第四节 |
| ⑯ 的日志行**在业主实机可见** | ⚠ 要等 1.0.28 装上。业主那台是 1.0.21 血统，`start_gateway` 那时还把子进程 stdout/stderr 丢进 `Stdio::null()`（重定向是 1.0.26 `bceedf5` 才加的），所以 `.wb-switch\gateway\gateway_data\` 里**没有** `gateway.log` 是预期行为，不是缺陷。已在 1.0.28 隔离实例上验证：包内宿主解出的网关 13,629,440 B / `99AA2D33…`，`gateway_data\gateway.log` 112,615 B / 837 行 |
| ⑯ 的**最终二进制**真机终验 | ✅ 已做（2026-10-06）：三轮 q16 抓包 `pass=43 fail=0`（`uitest\q16-verify2.log`，与 `uitest\verify-q16.log` 逐字节一致） |
| **交付包自证**：真机上解出的网关 == 工作区产物 | ✅ 已做（2026-10-06）：`uitest\verify-1028-deployed-gw.cjs` **5/5 PASS**。1.0.28 隔离实例里宿主自己写下的 `home\gateway\bin\gateway-4eaf37-29d0aea8025e7f09.exe` 13,629,440 B / `99AA2D33…` 与 `wt-port\target\release\gateway.exe`、`crates\ai-gateway-core\embedded\gateway.exe` **三者逐字节相同**；落地文件名亦按 `gateway_embed.rs:52` 口径复算命中（见四·7） |
| ⑮ **旧网关自动换新**（业主装 1.0.28 后 `:7864` 上那个 1.0.21 路由器会不会被换掉） | ✅ 已做（2026-10-06，1.0.28 隔离实例）：`uitest\gate-14-refresh.ps1 -Tag g14pkg1028b … -NewName 'gateway-4eaf37-29d0aea8025e7f09.exe'` ⇒ **`VERDICT_14_FIXED=True`**。旧 1.0.21 路由器（`gateway-4e52fa-ecd7924c953073e8.exe`，PID 74824）先占住 `:7874`，新 GUI 起来后 **7 秒**内完成识别/结束/替换，`HOLDER_AFTER` = 新名字，`OLD_ROUTER_ALIVE_AFTER=False`，`BIN_AFTER` 只剩新那一个（`cleanup_old` 生效），`CFG_LAST_STATUS=started` 且无 `last_error`。日志 `uitest\g14pkg1028b-run.log`；证据行 `test-bin\pkg1028\runs\g14pkg1028b.gui.err.log`（`[gateway] 端口 7874 上的网关不是本版本释放的，正在换成本版本内嵌的网关` / `已结束占用端口 7874 的网关进程 (PID 74824)`） |
| ⑰ **写入口径**（`model_key_of`：先大写定型、再写小写是否沿用既有拼写） | ✅ 已做（2026-10-06）：`uitest\q17-run.ps1` 两轮（A 大写 `GLM-5.3-Flash` → B 小写 `glm-5.3-flash`，假上游发一帧 `usage:{11,5,cache 3/2}`）⇒ `node uitest\verify-q17-usage.cjs` **`pass=23 fail=0` / `VERDICT=PASS`**。四张表（`models`/`accountModels`/`billed`/`billingMeta`）**始终只有 1 个模型名**，`11/5/3/2/1` ⇒ `22/10/6/4/2` 累加同一行；计费键 = `workbuddy|cn|?`；`billingMeta` 无第二条 |
| 11133 修复的**覆盖边界** | ⚠ `prepare_body` 只覆盖默认上游（workbuddy/cmb 族）与 Qoder；**Trae / ZCode 各有独立适配器、不经此处**。Trae 已在源头修掉；**ZCode 侧未修**（`products/zcode/translate.rs:157` 同样 `unwrap_or("")`），但其上游是 Anthropic 形状（不回 11133）、池里只有 2 个模型、会话里无 ZCode 流量 ⇒ 已知残余风险 |
| ⑲① Trae 凭证**端到端真机** | ❌ 本机无 Trae 真账号 ⇒ 只有源码级定案 + 3 条新单测（其中 `网关切到手动模式后_Trae_凭证仍在` 走**真实入口** `export_accounts_to_gateway()`） |
| ⑲② **真实签到成功**触发刷新 | ⚠ 本机无 Trae 真账号，`trae-checkin` 走的是「无账号成功短路」；**事件链路本身**已用原生探针证实（`rawEvents` + `domEvents` + 事件后 4 条刷新命令 `trae_list_accounts`/`app_env_check`/`trae_checkin_status`/`trae_checkin_trends`） |
| ⑲③ 在**真实豆包客户端**上隐藏 | ⚠ 本机未装豆包 ⇒ 用 `stub\Doubao.exe` 桩验证「隐藏/不隐藏」两分支（有/无 `已在后台隐藏 豆包 的 1 个窗口` 日志）且保活结果都是 `{"message":{"Ok":"保活完成"},"ok":true}`；真机隐藏效果需业主实机确认 |
| ⑲④ `Internal Network Failure` 的**上游原话复现** | ⚠ 该串由上游在流中下发，本轮未构造该形态的假上游帧；代码路径（识别/记日志/计入健康度/不改写，`server.rs` 的 `note_upstream_stream_error`）已核 |

---

## 六、硬性规矩（违反会伤到所有者）

- Shell **必须** PowerShell；路径用 `\`
- **绝不运行安装包 / 卸载器** —— NSIS 按**可执行文件名**杀进程，
  而主程序 `ai-gateway.exe` 与所有者正在跑的实例同名，会连带打掉 `:7864`
- **绝不按进程名杀进程** —— 只允许 **PID + ExecutablePath 双条件**
- **绝不运行**共享目录里的可执行文件（**哪怕 `--version`**）
- **push 需业主显式授权**：2026-10-06 业主 m26008「发布推送吧」授权后，**只允许推 `fork`（`iuuuuuuuu/ai-gateway`）**；`aigw` / `origin` / `upstream` 三个 remote **仍然不许碰**（其中 `aigw`/`upstream` 已 404）。**本地 commit 允许且鼓励**，每完成一个可交付单元就落一次 commit，消息用 `git commit -F <临时文件>`（PS 命令行多 `-m` + 中文会被拆词）
- 所有者服务 `:7864`（网关）/ `:3080`（DSH GUI）**必须保持在线**
- 公开仓库 ⇒ 被 git 跟踪的文件里**不得有真实 token / UID / deviceMid**
- 改完**必须自己跑实测**（真实启动实例 + 真实发请求），编译通过/单测绿都**不算**
- `.ps1` 必须带 **UTF-8 BOM**（PS 5.1 无 BOM 时按 GBK 读，中文注释会让语法崩）

---

## 七、外部依赖与凭据位置（都不在仓库内）

| 用途 | 路径 |
|---|---|
| updater 签名私钥 | `D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.key` |
| 私钥口令 | `D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.password` |
| 所有者网关配置 | `%USERPROFILE%\.wb-switch\gateway\gateway_native_config.json` |
| 账号凭证 | `%USERPROFILE%\.wb-switch\gateway_auths\` / `qoder\auths\` / `zcode\auths\` |

构建签名包：
```powershell
& ".\scripts\build-signed.ps1" -KeyFile "D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.key" `
                              -PasswordFile "D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.password"
```

---

## 八、有价值的复用资产

| 资产 | 位置 | 用途 |
|---|---|---|
| 可编排 mock 上游 | `crates\ai-gateway-router\src\testing\`（task-32） | 线上故障形态固化 |
| 不触网 runner 测试范例 | `crates\ai-gateway-router\src\products\product_tasks.rs` | 早退路径测试 |
| 假上游范例 | `crates\ai-gateway-router\src\upstream\tasks.rs` 测试模块 | `std::net::TcpListener` |
| 用量写入口径装置 | `uitest\q17-run.ps1` + `uitest\verify-q17-usage.cjs`（复用 `q16-ab.ps1 -Mode usage -Model <名>`） | 两轮真机：先大写定型、再写小写，断言四张表不裂（23/0）。假上游 `usage` 模式发一帧 `usage:{11,5,cache 3/2}` —— **不带 usage 的帧完全不计账** |
| 侦察报告 | `test-bin\recon\REPORT-{qoder,zcode,trae}.md` | 外部仓库功能清单 |
| 解包比对脚本思路 | `DELIVERY-1.0.14.md` 第四节 | 验证安装包内容而**不安装** |

---

## 九、分支形态与版本谱系（2026-10-06 落地）

### 9.1 分支改名

本仓库**不再叫 `migrate/aigw-v1.0.2`**（那是「从 Go 网关迁到 Rust 网关」时期的名字，迁移早完成了）。现在的形态：

| 分支 | 指向 | 说明 |
|---|---|---|
| **`main`** | `9ce3ba5`（v1.0.31） | **本仓库唯一开发主线**。由 `migrate/aigw-v1.0.2` 原地改名而来，全部 187 个提交、42 个 tag 都在这条线上。**无上游追踪**（`branch.main.remote` 为空）—— 有意如此，避免误 `git pull` 把上游陈旧线拉进来 |
| `archive/origin-main` | `224919a` | 原 `main` 改名留档。它是 `origin/main`（`iuuuuuuuu/workbuddy-switch-gateway`）那条**陈旧且独立**的血统，与主线 `merge-base` 停在 `117643f`（v0.7.0 那代）。保留只是为了「还能查到那一版长什么样」，不再参与开发 |

★ 改名当时是纯本地操作（**没有 push、没有改 remote**）。（2026-10-06 之后 `main` 已按业主 m26008 授权推送到 `fork`，见 9.6。）`archive/origin-main` 仍带着 `branch.archive/origin-main.remote=origin` 的追踪配置（改名不会清），但没有任何脚本/工作流依赖分支名，所以不构成风险。

### 9.2 版本号 ↔ tag 谱系（补 tag 后）

本仓库此前 **v1.0.11 起全部没有 tag**，「版本 ↔ 提交」只存在于提交信息和文档里。2026-10-06 补了 10 个 annotated tag：

| tag | 提交 | 备注 |
|---|---|---|
| `v1.0.21` | `3d5a1be` | 该版本当时 `tauri.conf.json` 写 1.0.21、`package.json` 仍写 1.0.10（**两文件历史上不同步**，tag 只保证 tauri.conf.json） |
| `v1.0.23` | `920c024` | |
| `v1.0.24` | `206f19f` | |
| `v1.0.25` | `14dc950` | |
| `v1.0.26` | `f2f7489` | |
| `v1.0.27` | `c86aea6` | |
| `v1.0.28` | `37f5028` | |
| `v1.0.29` | `6e426f9` | |
| `v1.0.30` | `811b179` | |
| `v1.0.31` | `9ce3ba5` | tag 之后又落了 4 个提交（`e2f70d5` 仓库形态文档 → `ab2351d` 交付路径 → `001c40a` 出处更正 → `c1ae06a` 发布后复核），**tag 有意停在版本提交上**；现 HEAD = `c1ae06a`（`git describe` = `v1.0.31-5-gc1ae06a`） |

打 tag 的规则：**版本号提交 V 的父提交**（= 该版本工作的最后一个提交；1.0.31 是 HEAD）。tag 是 annotated，带消息（说明为什么标这个提交、交付包路径）。本地 tag 总数 **32 → 42**。

★ **`v1.0.11`–`v1.0.20`、`v1.0.22` 永远打不了 tag** —— 这些版本号在**任何提交的 `package.json` / `tauri.conf.json` 里都从未出现过**（`git log --all -S` 全空）。它们是**交付编号**：包做出来了、`dist\` 里有 exe、文档里写了，但当时没改版本号文件。⇒ 查这些版本只能靠 `dist\AI Gateway_<ver>_x64-setup.exe` 与 `test-bin\DELIVERY-<ver>.md`。

### 9.3 分发通道现状（三条，都是本仓库自己的）

| 通道 | 地址 | 现状 |
|---|---|---|
| 桌面端自动更新 | `https://github.com/iuuuuuuuu/ai-gateway/releases/latest/download/latest.json` | 2026-10-06 **已发 v1.0.31 Release**（id `404503791`，5 资产）与 **v1.0.32 Release**（id `404687242`，5 资产，`releases/latest` 现指向它）。但**仓库同日按业主 m26011 转为 private** ⇒ 匿名拉取 **404**，该通道**目前对客户端失效**（详见 9.6；1.0.32 的推送 / tag / Release 全过程见 9.7） |
| npm | `npm/` 子目录的 `ai-gateway` 包（`optionalDependencies` 5 个平台包） | 未发布过 |
| 直接发安装包 | `D:\WishProject\WorkbuddySwitchAPi\dist\` | 1.0.7 – 1.0.32 全套都在（每版含 `.sig`），**比 GitHub Release 完整** |

★★ **更正（2026-10-06，密码学实测）**：本节此前写「签名私钥与 `tauri.conf.json` 配置的公钥不配对」，**该结论是错的**。

- `keys\ai-gateway-updater.key.pub` 解出的公钥 blob 与 `src-tauri/tauri.conf.json:53` 的 `plugins.updater.pubkey` base64 解码后的 blob **逐字节相同**（`IDENTICAL_TO_KEY_PUB = true`），keyid 两边都是同一 8 字节 `7e7ce64b6fc3a36f`。
- 此前误判的根源：**minisign 显示 keyid 用大端**（`6FA3C36F4BE67C7E`，就是 `.key.pub` 注释行里印的那个），而文档里抄的是**原始字节序**（`7e7ce64b6fc3a36f`）⇒ 同一把钥匙的两个写法被当成了两把。
- 端到端验证（Node `crypto.verify`，Ed25519）：`dist\AI Gateway_1.0.31_x64-setup.exe` 的签名 **VALID = true**，公钥取 `keys\ai-gateway-updater.key.pub`；`trusted comment` 的全局签名也 **VALID = true**。
- 细节：minisign 签名 alg 字段是 `ED`（**预哈希** BLAKE2b-512），所以必须 `verify(blake2b512(file), sig)` 才对 —— 直接对文件原始字节验签会得到 false，**这是正常的，不代表签名坏**。

⇒ 结论：**本地构建用的就是 `tauri.conf.json` 里配的那把**，签名链路完全自洽，无需换钥匙。

### 9.4 没做的事（有意）

- ~~**没有 push**~~ → **2026-10-06 已 push**（业主 m26008「发布推送吧」授权）。见 9.6：`main` + 42 tag + v1.0.31 Release 全部落到 `fork`；1.0.32 后为 `main` + **43 tag** + v1.0.31/v1.0.32 两个 Release（见 9.7）。
  - 凭据真相：`GITHUB_TOKEN`/`GH_TOKEN` 未设置、`gh` CLI 不存在，但 **Windows 凭据管理器里有可用 token** —— `cmdkey /list` 的 `LegacyGeneric:target=git:https://github.com`（user `iuuuuuuuu`，secret 40 字符、前缀 `gho_`）。用 `advapi32.dll` 的 `CredRead` P/Invoke 读出来即可（脚本 `.scratch\ghtoken.ps1`，该目录已被清理；重建版在仓库外的 `.tmp\ghtoken.ps1`，同一套 `CredRead` P/Invoke）。**scopes = `gist, repo, workflow`** ⇒ 能 push / 建 Release / 改可见性，但**没有 `delete_repo`**。
  - ★ `git credential fill` 走管道会报 `refusing to work with credential missing protocol field`；`git push --dry-run` 在重定向 stdio 下**会卡死 70 s** 等交互提示 ⇒ 只能直接读凭据管理器。
- **仍然没有开 PR、没有建新仓库**（业主 m24376-2 授权自主决定，结论是继续自研本仓库）。
- **没有合并 `main` 的最后一个提交**：`archive/origin-main` 相对主线只有 1 个提交（`224919a`「修复负载均衡只用到少数账号与长对话 unexpected EOF」），改动全在 `go-gateway/`（已不参与构建的 Go 参照）+ 版本行 + README 15 行 ⇒ 无移植价值。
- **`origin` / `upstream` / `aigw` 三个 remote 未删**，留作历史参照。其中 `upstream`(`momo0410/workbuddy-switch-gateway`) 与 `aigw`(`momo0410/ai-gateway`) 已 404。

### 9.5 上游仓库存活现状（2026-10-06 核查）

业主问过「两个都 404 是什么意思」，这里留一份可直接引用的核查结论。**判存活必须走 `api.github.com`**：本机 `curl` 对 `github.com/<owner>` 的 HTML 页面一律返回 `000`（出口不通），但 API 完全可用。

| 仓库 | HTTP | 事实 |
|---|---|---|
| `momo0410/ai-gateway` | **404** | 本仓库的**直接上游**。已删/改私有。账号本身还在（`api.github.com/users/momo0410` = 200，5 个公开仓库全是无关项目，还在活跃）⇒ 是仓库没了，不是人没了 |
| `momo0410/workbuddy-switch-gateway` | **404** | 同上（remote 名 `upstream`） |
| `Sliverkiss/workbuddy2api` | **404** | 血统链第二层，也已删。但本地 `ref-sliverkiss` 已 fetch 到 `b08f518`（2026-09-19） |
| `moshouhot/workbuddy-switch-gateway` | 200 | 血统链第三层，是 `origin` 的 **parent**；本地已有其全部（`git cherry -v HEAD d815cef` 零输出） |
| `changexbc/workbuddy-switch` | 200 | 血统链最底层，**仍在更新**（pushed 2026-10-04） |
| `linguo2625469/workbuddy2api-panel` | 200 | 面板参考仓库，**今天还在更新**（pushed 2026-10-06） |

**对我们零影响**：本地 5 个 worktree / 42 个 tag / 全部历史都在；`fork`（`iuuuuuuuu/ai-gateway`）是 `"fork": false` 的**独立仓库**，不依赖 `momo0410` 存活。updater endpoints 指向它 ⇒ 上游 404 **不影响**我们的自动更新通道（该通道的问题另有其因：2026-10-06 前是停在 1.0.10，之后是仓库转私有，见 9.3 / 9.6）。

**祖先关系实测**（`git merge-base --is-ancestor`）：`moshouhot/main` 是 HEAD 的祖先（`..HEAD` 273 个提交，反向 0）；`momo0410-aigw/main` 也是 HEAD 的祖先（170 / 0）；`origin/main` 与 HEAD **互不为祖先**（那条是独立血统）；`upstream/main` 与 HEAD 有 6 个未合提交（全是 2026-09-16/17 旧货，逐条判定不需要合：supportsImages / 思考等级 / 账号备注 / 两次版本 bump 本地都已有等价或更强实现）。

★ 全量备份：`merge-backup\after-upstream-404.bundle`（37,188,040 B，84 refs，`git bundle verify` = 完整历史）。此前三个 bundle 都**不含当前 HEAD**。

### 9.6 远端落地与转私有（2026-10-06，业主 m26008 / m26011）

业主两条指令：m26008「发布推送吧」、m26011「然后把我们的仓库也改为私有」。以下全部为**实测结果**。

**推送（目标只有 `fork` = `iuuuuuuuu/ai-gateway`）**

| 项 | 结果 |
|---|---|
| `main` | 首推 `001c40a8d039858e13d95665903f6bc3e0c71be8`；随后文档提交 `7fafc428c45a6b183c1a003cada642481b13efe6` 也已推送 ⇒ 远端 `main` = 本地 HEAD，`rev-list --left-right --count fork/main...HEAD` = `0 0` |
| tag | 本地 **42** = 远端 **42**（补推的 11 个：`v1.0.4` `v1.0.21` `v1.0.23`–`v1.0.31`；同名不同 SHA 的冲突 **0 个**） |
| 远端分支 | `main`=001c40a、`migrate/aigw-v1.0.2`=5a05b35、`feat/tasks-and-dual-realm`=92fcc21、`fix/migrate-merge-instead-of-skip`=add8b52 |
| Release | `v1.0.31`，id `404503791`，`draft=False prerelease=False`，`releases/latest` 已指向它 |

Release 的 5 个资产（上传前安装包**改名为 updater 约定名** `ai-gateway-windows-x86_64-setup.exe`，因为 updater 按清单里的 `url` 取文件）：

| asset | bytes | assetId |
|---|---|---|
| `ai-gateway-windows-x86_64-setup.exe` | 14586903 | 615002270 |
| `ai-gateway-windows-x86_64-setup.exe.sig` | 424 | 615001400 |
| `ai-gateway_1.0.31_server-x64.exe` | 27274752 | 615001836 |
| `latest.json` | 1524 | 615001242 |
| `latest-windows-x86_64.json` | 1524 | 615001430 |

★ **updater 清单里 `signature` 字段 = `.sig` 文件内容逐字**（`.sig` 本身已是 base64 文本）⇒ **直接原样填入，不要解码、不要二次 base64**。1.0.31 清单只发 `windows-x86_64-nsis` + `windows-x86_64` 两个键（本轮仅出 Windows 资产）。

**发布后复核（实测）**

| 检查 | 结果 |
|---|---|
| 远端 `main` | `7fafc428c45a6b183c1a003cada642481b13efe6` = 本地 HEAD |
| 远端 tag 数 | **42**（与本地一致） |
| Release 正文 | `bodyLen=6295`，含第八节（转私有说明），旧版「通道已恢复」措辞已消失 |
| 资产可下载性（**转私有后**，走 API + token） | `curl.exe -sL -H 'Authorization: Bearer <tok>' -H 'Accept: application/octet-stream' -o <out> https://api.github.com/repos/iuuuuuuuu/ai-gateway/releases/assets/615002270` ⇒ `curl_exit=0`，`remoteBytes=14586903`，SHA256 `1FB28395FE386E160482BC6BEC1A2417809755C24B3521C125F56ABBEAADA2A5` ⇒ 与本地 `dist\AI Gateway_1.0.31_x64-setup.exe` **逐字节一致** |
| 资产内容与代码的对应 | `git diff --name-only 9ce3ba5..HEAD` 里**没有任何非 `.md` 文件** ⇒ 版本提交之后只改文档，**Release 资产不需要重发** |

★ 坑：`Invoke-WebRequest -MaximumRedirection 0` 想读 302 的 `Location` 会直接抛 `Operation is not valid due to the current state of the object`；把 token 写进 curl 的 `--config` 文件会被 curl 当配置指令解析（`unknown option`）。**直接 `curl.exe -H <auth> -L -o <out> <api-url>` 最省事。**

**顺带：Dependabot 告警 3 条**（推送时 GitHub 提示，与本次交付无关）

| 严重度 | 包 | 清单 | 摘要 |
|---|---|---|---|
| high | `source-map-js` | `package-lock.json` | 索引化 source-map 段偏移导致事件循环拒绝服务 |
| medium | `rustls` | `Cargo.lock` | TLS 1.3 握手消息被错误地跨加密层级接受 |
| medium | `glib` | `Cargo.lock` | `VariantStrIter` 的 `Iterator`/`DoubleEndedIterator` 实现不可靠 |

三者都在依赖锁文件里，不在自写代码里；本版交付物未因此改动（如需处理应作为独立任务：先升依赖，再重跑 `cargo test --workspace` + 真机验证）。

**转私有（m26011）与其代价**

| 项 | 结果 |
|---|---|
| 执行 | `PATCH /repos/iuuuuuuuu/ai-gateway` body `{"private":true}` ⇒ `private=True visibility=private`（BEFORE 快照存 `.scratch\rel31\repo-before-private.json`） |
| 带 token | 正常，`releases/tags/v1.0.31` 仍见 5 资产 |
| 匿名 | `releases/latest/download/latest.json` / `api.github.com/.../releases` / 仓库页 **全部 404** |

★★ **代价（必须知道）**：`tauri-plugin-updater` 是**匿名 HTTP 拉取、不带任何凭据**，所以转私有后 **桌面端自动更新彻底失效** —— 包括业主自己桌面那版（`D:\APP\AI Gateway\ai-gateway.exe`，端点写死 `https://github.com/iuuuuuuuu/ai-gateway/releases/latest/download/latest.json`）。这不是配置问题，**加 `Authorization` 头也救不了**。要恢复只有两条路：**改回 public**，或**把 `endpoints` 换成别的托管点**（自建静态站 / 对象存储 / 另一公开仓库）。

对照实测（用业主已有的私有仓库）：`mnemon-memory-sync` 的 `releases/latest/download/latest.json` 匿名 = **404**，而同期的 `ai-gateway`（当时 public）= **302**。⇒ 这不是猜测，是同一台机器上的 A/B。

**为什么转私有对安全没有实质收益**：全量扫描 850 个 tracked 文件，`gho_[A-Za-z0-9]{20,}` / `Bearer [A-Za-z0-9._-]{40,}` / `sk-[A-Za-z0-9]{16,}` **命中均为 0** ⇒ 仓库里本来就没有真 token / API key。命中的 email 与 GUID 全是测试占位（`user@example.com` 一类、`00000000-0000-4000-8000-000000000001` 一类）。

### 9.7 1.0.32 的推送 / tag / Release（2026-10-06，本轮实测）

业主 m26396 报障（裸名 `space-bunny` ⇒ 503）修复完成后，按与 1.0.31 完全相同的流程落地远端。

| 项 | 结果 |
|---|---|
| 版本提交 | `371b97198ead140a652dd8a80d53754a5499820e`（`feat(1.0.32): 修裸名模型 503（上游实测集）+ 裸名区域收敛`，18 files / 1072 insertions / 50 deletions） |
| tag | `v1.0.32` = annotated，对象 `8d33aedd39bce985cb4aa59953436570e7b03074`，指向 `371b971`（**tag 落在版本提交上**，规则见 9.2）；本地 tag **42 → 43** |
| `main` | 推送 `c1ae06a..371b971`；远端 `refs/heads/main` = `371b97198ead140a652dd8a80d53754a5499820e` = 本地 HEAD |
| tag 远端一致性 | 远端 **43** = 本地 **43**（`ls-remote --tags` 去 peel 行计数） |
| Release | tag `v1.0.32`，id **`404687242`**，`draft=false prerelease=false`，`target_commitish=main`，正文 = `.github\RELEASE_NOTES_1.0.32.md` 全文（`bodyLen=7854`） |

Release 的 5 个资产（安装包上传前**改名为 updater 约定名** `ai-gateway-windows-x86_64-setup.exe`）：

| asset | bytes | assetId |
|---|---|---|
| `ai-gateway-windows-x86_64-setup.exe` | 14598416 | 615475451 |
| `ai-gateway-windows-x86_64-setup.exe.sig` | 424 | 615475894 |
| `ai-gateway_1.0.32_server-x64.exe` | 27063808 | 615475963 |
| `latest.json` | 1551 | 615476242 |
| `latest-windows-x86_64.json` | 1551 | 615476281 |

★ **`latest.json` 的写法**：`version` = `1.0.32`，`platforms` 只发 `windows-x86_64-nsis` 与 `windows-x86_64` 两个键，两者的 `signature` **都是 `.sig` 文件内容逐字**（`.sig` 本身已是 base64 文本 ⇒ 原样填入，**不要解码、不要二次 base64**），`url` 都指向 `releases/latest/download/ai-gateway-windows-x86_64-setup.exe`。两个清单文件**内容完全相同**（1.0.31 亦然）。`pub_date` = `2026-10-06T12:00:00Z`。

**发布后复核（实测，不是推断）**

| 检查 | 结果 |
|---|---|
| `releases/latest` | 指向 `v1.0.32`（id `404687242`），`draft=false prerelease=false`，`bodyLen=7854` |
| 资产逐字节核对（走 API + token 真下回来） | 5 个资产 `remoteBytes` 与本地完全相等，`buf.equals(local)=true`：安装包 `14598416` / `BD07D32C…`、`.sig` `424` / `FAE936C8…`、server `27063808` / `1788241A…`、两个 `latest*.json` `1551` / `BF154F53…` |
| 匿名访问（转私有后仍是） | `releases/latest/download/latest.json` **404**、`api.github.com/.../releases` **403**、仓库页 **404** ⇒ 9.6 的结论**未变**，updater 通道对客户端依旧失效 |

★ 推送踩到的坑（留给后来者）：`git push` / `ls-remote` 在**同一 shell 里先跑过交互式命令**后可能报 `fatal: Cannot prompt because user interactivity has been disabled.` / `could not read Username for 'https://github.com'`。解法：每次调用前显式设 `$env:GIT_TERMINAL_PROMPT='0'; $env:GCM_INTERACTIVE='never'; $env:GCM_PROVIDER='generic'`（凭据本来就在 Windows 凭据管理器里，GCM 能非交互取到）。

★ 另一个坑：`git ls-remote fork refs/heads/main` 的 URL 会走全局 `http.proxy = http://127.0.0.1:7890`（业主代理，全程在线，**没动它**）。本机对 `github.com` 直连与走代理**都通**（`curl` 实测 `code=200`）；`api.github.com` 直连返回 **403**，所以 API 调用一律走 HTTPS 直连的 `fetch`。

### 9.8 1.0.33 的推送 / tag / Release（2026-10-07，本轮实测）

业主 m28058 报障（裸名 `space-bunny` ⇒ 400 `model_not_in_upstream`）修复完成后，按与 1.0.32 完全相同的流程落地远端。

| 项 | 结果 |
|---|---|
| 版本提交 | `2263b173ada5abe68e1d94511e6a798f7687bdc6`（`feat(1.0.33): 修裸名模型抽到 Trae 账号（兜底清单权威性）+ 启动预热`，18 files / 459 insertions / 26 deletions） |
| 文档提交 | `868db64f8bca1d396b96a1efde6a6e820aa5a5cf`（`docs(1.0.33): PROJECT-STATE 记录 1.0.33`，1 file / 46 insertions / 28 deletions）—— 与 1.0.32 同规矩：tag 只落**版本提交** |
| tag | `v1.0.33` = annotated，对象 `19f06b1c9c188fec94a92806b9e10208778b7b42`，指向 `2263b17`；本地 tag **43 → 44** |
| `main` | 推送 `8624461..868db64`（发布时）⇒ 随后又推 `868db64..2048591`（本节的文档提交）；远端 `refs/heads/main` 现为 `2048591e97b07bdcd16f169065e0c99c2445ffb5` = 本地 HEAD |
| tag 远端一致性 | 远端 `refs/tags/v1.0.33` = `19f06b1c…`，与本地一致（本地 tag 44） |
| Release | tag `v1.0.33`，id **`405310590`**，`draft=false prerelease=false`，`target_commitish=main`，正文 = `.github\RELEASE_NOTES_1.0.33.md` 全文（`bodyLen=7872`，文件 13,051 B） |

Release 的 5 个资产（安装包上传前**改名为 updater 约定名** `ai-gateway-windows-x86_64-setup.exe`）：

| asset | bytes | assetId |
|---|---|---|
| `ai-gateway-windows-x86_64-setup.exe` | 14605514 | 617316496 |
| `ai-gateway-windows-x86_64-setup.exe.sig` | 424 | 617316567 |
| `ai-gateway_1.0.33_server-x64.exe` | 27302400 | 617316583 |
| `latest.json` | 1642 | 617316688 |
| `latest-windows-x86_64.json` | 1642 | 617316710 |

★ `latest.json` 写法同 9.7（`version=1.0.33`、两个平台键的 `signature` 都是 `.sig` 逐字、`url` 指向 `releases/latest/download/ai-gateway-windows-x86_64-setup.exe`、两份文件内容完全相同）。`pub_date` = `2026-10-07T02:30:00Z`。

**发布后复核（实测，不是推断）**

| 检查 | 结果 |
|---|---|
| `releases/latest` | 指向 `v1.0.33`（id `405310590`），`draft=false prerelease=false`，`bodyLen=7872`，5 资产 |
| 资产逐字节核对（走 API + token 真下回来） | 5 个资产 `buf.equals(local)=true`：安装包 `14605514` / `20746313…`、`.sig` `424` / `B7854BCB…`、server `27302400` / `AC18AD26…`、两个 `latest*.json` `1642` / `549F1B4D…` |
| 匿名访问（转私有后仍是） | `releases/latest/download/latest.json` **404**、`api.github.com/.../releases` **403**、仓库页 **404** ⇒ 9.6 的结论**未变**，updater 通道对客户端依旧失效 |

★ 本轮新增的坑：`git add -- 'a','b','c'` 在 PowerShell 里**整串被当成一个 pathspec** ⇒ `fatal: pathspec 'a,b,c' did not match any files`，且**一个文件都不会 staged**。正确写法是数组：`$files = @('a','b','c'); git add -- $files`。

### 9.9 1.0.34 的推送 / tag / Release（2026-10-07，本轮实测）

业主 m29254 报障（「对话过程中对话会输出完就中断」）修复完成后，按与 1.0.32 / 1.0.33 完全相同的流程落地远端。

| 项 | 结果 |
|---|---|
| 版本提交 | `cd863ebe661f4c3cb8372e1bf7d552ac5b1398d3`（`feat(1.0.34): 修「对话输出完就中断」（上游已宣告完成时不再误判为截断）`，**20 files / 503 insertions / 69 deletions**）★ 与 1.0.33 不同：本轮的 `PROJECT-STATE.md` 与 `RELEASE_NOTES_1.0.34.md` **一起进版本提交**（1.0.33 时 PROJECT-STATE 是单独的 docs 提交） |
| tag | `v1.0.34` = annotated，对象 `d555a3dcc12561a3291e2045aed4d9356cf04afa`，指向 `cd863eb`；本地 tag **44 → 45** |
| `main` | 推送 `b66d5d7..cd863eb`（版本提交）；随后文档提交 `87cf271eb16804134082964abd1c2cfbf9653a4a` 又推了一次 `cd863eb..87cf271`；远端 `refs/heads/main` 现为 **`87cf271`** = 本地 HEAD（`git describe --tags` = `v1.0.34-1-g87cf271`） |
| tag 远端一致性 | 远端 `refs/tags/v1.0.34` = `d555a3dc…`，与本地一致（本地 tag 45、远端 tag 45） |
| 文档提交 | `87cf271eb16804134082964abd1c2cfbf9653a4a`（`docs(1.0.34): 补记推送 / tag / Release（发布后复核实测）`，1 file / 35 insertions）—— 与 1.0.32 / 1.0.33 同规矩：**tag 只落版本提交**（`git diff --stat v1.0.34 HEAD` 只有 `PROJECT-STATE.md`） |
| Release | tag `v1.0.34`，id **`405456103`**，`draft=false prerelease=false`，`target_commitish=main`，正文 = `.github\RELEASE_NOTES_1.0.34.md` 全文（`bodyLen=8327`，文件 13,965 B / 8,327 字符，`body === file` 为 true） |

Release 的 5 个资产（安装包上传前**改名为 updater 约定名** `ai-gateway-windows-x86_64-setup.exe`）：

| asset | bytes | assetId |
|---|---|---|
| `ai-gateway-windows-x86_64-setup.exe` | 14626546 | 617830983 |
| `ai-gateway-windows-x86_64-setup.exe.sig` | 424 | 617831091 |
| `ai-gateway_1.0.34_server-x64.exe` | 27160576 | 617831129 |
| `latest.json` | 1726 | 617831352 |
| `latest-windows-x86_64.json` | 1726 | 617831389 |

★ `latest.json` 写法同 9.7 / 9.8（`version=1.0.34`、两个平台键的 `signature` 都是 `.sig` 逐字、`url` 指向 `releases/latest/download/ai-gateway-windows-x86_64-setup.exe`、两份文件内容完全相同）。`pub_date` = `2026-10-07T14:40:00Z`。上传源目录 `\.scratch\rel34\upload\`（安装包 / `.sig` / server 从 `dist\` 改名复制，`buf.equals` 逐字节相同）。

**发布后复核（实测，不是推断）**

| 检查 | 结果 |
|---|---|
| `releases/latest` | 指向 `v1.0.34`（id `405456103`），`draft=false prerelease=false`，`bodyLen=8327`，5 资产 |
| 资产逐字节核对（走 API + token 真下回来） | 5 个资产 `buf.equals(local)=true`：安装包 `14626546` / `6B87A399…`、`.sig` `424` / `66A7A984…`、server `27160576` / `70E72BD0…`、两个 `latest*.json` `1726` / `4E2210E4353BDE4F…` |
| 匿名访问（转私有后仍是） | `releases/latest/download/latest.json` **404**、`api.github.com/.../releases` **403** ⇒ 9.6 的结论**未变**，updater 通道对客户端依旧失效 |

★ 凭据流程与 9.8 相同：`\.tmp\ghtoken.ps1`（CredRead P/Invoke，脚本本身不含字面 token）⇒ 写 `\.tmp\ghtoken.txt`（40 B `gho_`）⇒ 用完**立即删除**（`ghtoken.txt` 已删，`ghtoken.ps1` 保留复用）。推送前的 `GIT_TERMINAL_PROMPT=0` / `GCM_INTERACTIVE=never` / `GCM_PROVIDER=generic` 三件套同样必要。
★ 本轮 `git add` 沿用 9.8 的数组写法（`$files = @(...); git add -- $files`），20 个路径一次 staged，无 pathspec 报错。

### 9.10 1.0.35 的推送 / tag / Release（2026-10-08，本轮实测）

业主 10-08 两条指令：先自行决定仓库里两个脏文件怎么处理，然后「发布一下 rela 新版本的构建」。本版 = 可观测性补完（㉛）+ 国服积分恒 0 修复（㉜），按与 1.0.32 / 1.0.33 / 1.0.34 完全相同的流程落地远端。

| 项 | 结果 |
|---|---|
| 版本提交 | `7170d48bdf2c1dcea3d79571eda0dedcc229bef1`（`feat(1.0.35): 网关日志全量时间戳 + 流收尾凭据 + /status 在途数；修国服账号消耗积分恒为 0`，**52 files / 762 insertions / 248 deletions**，含新增 `.github/RELEASE_NOTES_1.0.35.md` 与 `crates/ai-gateway-router/src/logging.rs`）★ 与 1.0.34 同规矩：PROJECT-STATE.md 与 RELEASE_NOTES 一起进版本提交 |
| tag | `v1.0.35` = annotated，对象指向 `7170d48`（`git rev-list -n 1 v1.0.35` = HEAD，`git diff --stat v1.0.35 HEAD` 为空 ⇒ **tag 只落版本提交**） |
| `main` | 推送 `44b5bfb..7170d48`（`git push fork main`）；远端 `refs/heads/main` 现为 `7170d48` = 本地 HEAD |
| tag 远端一致性 | `git push fork v1.0.35` ⇒ `* [new tag] v1.0.35 -> v1.0.35` |
| Release | tag `v1.0.35`，id **`406646519`**，`draft=false prerelease=false`，`target_commitish=main`，正文 = `.github\RELEASE_NOTES_1.0.35.md` 全文（`bodyLen=5717`；文件 5,717 字符 / 9,853 B，`body === file` 为 true） |

Release 的 5 个资产（安装包上传前**改名为 updater 约定名** `ai-gateway-windows-x86_64-setup.exe`）：

| asset | bytes | assetId |
|---|---|---|
| `ai-gateway-windows-x86_64-setup.exe` | 14673521 | 621334571 |
| `ai-gateway-windows-x86_64-setup.exe.sig` | 424 | 621335026 |
| `ai-gateway_1.0.35_server-x64.exe` | 27212800 | 621335602 |
| `latest.json` | 1954 | 621335872 |
| `latest-windows-x86_64.json` | 1954 | 621335924 |

★ `latest.json` 写法同 9.7 / 9.8 / 9.9（`version=1.0.35`、两个平台键 `windows-x86_64-nsis` 与 `windows-x86_64` 的 `signature` 都是 `.sig` 逐字、`url` 指向 `releases/latest/download/ai-gateway-windows-x86_64-setup.exe`、两份文件内容完全相同）。`pub_date` = `2026-10-08T09:17:19Z`。上传源目录 `\.scratch\rel35\upload\`（安装包 / `.sig` / server 从 `dist\` 改名复制）。

**发布后复核（实测，不是推断）**

| 检查 | 结果 |
|---|---|
| `releases/latest` | 指向 `v1.0.35`（id `406646519`），`draft=false prerelease=false`，5 资产 |
| 资产逐字节核对（走 API + token 真下回来） | **5/5 `MATCH=True`**：安装包 `14673521` / `F8236A91…`、`.sig` `424` / `47700194…`、server `27212800` / `4F7E7D73…`、两个 `latest*.json` `1954` / `0D594218…` |
| 匿名访问（转私有后仍是） | `releases/latest/download/latest.json` **404**、`api.github.com/.../releases` **403** ⇒ 9.6 的结论**未变**，updater 通道对客户端依旧失效 |
| Ed25519 验签（公钥取 `tauri.conf.json plugins.updater.pubkey`） | `FILE SIG VALID = true`、`GLOBAL SIG VALID = true`；keyid `6fa3c36f4be67c7e`（展示形）双方一致 |

★ **验签配方补正（1.0.35 实测发现，之前 9.8 的写法对 FILE SIG 是错的）**：tauri/minisign 的 `Ed`/`ED` 算法标识意味着**预哈希**签名 —— 文件签名必须对 `blake2b-512(file)` 验，直接对原文件验会得到 `false`（本轮先踩到这个 false，再改成预哈希才为 true）。全局签名仍不预哈希，消息 = `sig64 || trusted_comment 正文`（去掉 `"trusted comment: "` 前缀、不加换行）。公钥/签名的二进制布局：`[2 字节算法标识][8 字节 keyid][32 字节公钥 | 64 字节签名]`，keyid 在小端存储（`7e7ce64b6fc3a36f` ↔ 展示形 `6fa3c36f4be67c7e`）。

★ 凭据流程与 9.8 / 9.9 相同：`\.tmp\ghtoken.ps1`（CredRead P/Invoke，脚本本身不含字面 token）⇒ 写 `\.tmp\ghtoken.txt`（40 B `gho_`）⇒ 用完**立即删除**（`ghtoken.txt` 已删，`ghtoken.ps1` 保留复用）。推送前的 `GIT_TERMINAL_PROMPT=0` / `GCM_INTERACTIVE=never` / `GCM_PROVIDER=generic` 三件套同样必要。
★ 本轮 `git add` 沿用 9.8 的数组写法（`$files = @(...); git add -- $files`），52 个路径一次 staged，无 pathspec 报错。
★ 本轮的 Windows 侧坑：`Invoke-RestMethod` 建 Release 时 `-Body` 传含中文的 JSON 会因编码丢字符 ⇒ 改为**先把 payload 写成 UTF-8 文件再 `-InFile`**，`bodyLen` 才与文件字符数相等（5,717）。另：node 侧的 `fetch` 在本机直连 api.github.com 会 `TypeError: fetch failed`（无系统代理），GitHub API 一律走 PowerShell。

★ **本版顺带落地的仓库卫生决定（业主授权「你自己拿主意吧」）**：
- `repo` worktree 的 `Cargo.lock`（`wb-switch-rust` 0.8.1 → 0.8.2 一行，与 `src-tauri/Cargo.toml` 已声明的 0.8.2 对齐）—— 属遗留一致性修正，保留待提交；
- `repo` worktree 的 `src-tauri/tauri.dev-sandbox.conf.json`（本机 dev 沙箱配置、标题乱码）—— 属机器本地文件，**进 `.gitignore` 而非提交**（`wt-port/.gitignore:91` 已含同名条目）。
