## AI Gateway 1.0.14 · 正式版

这是一个**架构换代版**：网关从 Go 全量重写为 **Rust**，并补回重写过程中丢失的三处**启动期接线**。

> ⚠ **如果你装的是 1.0.13 或更早**：请升级到 1.0.14。1.0.13 的网关是 **Go** 实现（`gateway-3f4554-f4646e258096b2c1.exe`）；1.0.14 落地的是 **Rust** 实现，文件名会变，旧文件由 `gateway_embed::cleanup_old()` 自动清理。
>
> ⚠ **如果你装过 1.0.14 的早期构建**：那一版**内嵌的 Rust 网关缺三处启动接线**（详见第三节），表现为「Qoder 定时领取不触发」「ZCode 自动领取完全不跑」「CLI 子命令让界面卡死」。本版是**取代它的修复版**。

---

## 一、网关换成 Rust 全量重写

按你的决定（「我们也换成rust全重写」），网关由 Go 全量重写为 Rust，但走的是**多产品等价**路线 —— 不是上游 PR#2 那条「删掉 Go 网关、不接 Qoder、ZCode 只作为可导入客户端」的路线。

| | 1.0.13 | 1.0.14 |
|---|---|---|
| 网关实现 | Go | **Rust** |
| 落地文件名 | `gateway-3f4554-f4646e258096b2c1.exe` | `gateway-*.exe`（新指纹） |
| 多产品（Qoder / ZCode） | ✅ | ✅ 等价保留 |
| 构建链是否还依赖 Go 工具链 | 是 | **否** |

Rust 侧新增/移植的模块：`ai-gateway-router` 25 个 `pub mod`，含 `auth` / `pool` / `server` / `forward` / `scheduler` / `growtask` / `usage` / `products/{qoder,zcode}` 等；`products/zcode` 下另有 `claim.rs`、`claim_scheduler.rs`、`warm.rs`、`login.rs`、`plan.rs`、`cli.rs`。

---

## 二、把 Go 的 `main.go` 启动接线逐条对齐（本次最有价值的部分）

**症状的共同形状**：Rust 重写搬了**实现**、丢了**接线**，而所有接线点都在 **HTTP 面之外** —— 所以 43 项 HTTP 验收**一条都抓不到**。

可复用的方法：**把 Go `main.go` 的启动接线清单与 Rust `bin/gateway.rs` 的逐条对差**。

### 2.1 CLI 子命令整个缺失（P0，会让界面卡死）

Go 版 `main()` 第一件事是拦截子命令；Rust 版没有这段 ⇒ 宿主 `Command::output()` 会**永久挂住**。

**影响面**：宿主有 **12 处** `qoder-login` + **13 处** `zcode-login` 调用点，包括**20 分钟一次的后台余额巡检**（`multi_product_credit_patrol.rs:47` `PATROL_INTERVAL = 20*60s`）、账号页「刷新」、ZCode「激活」按钮。

**真机复现**（1.0.14 早期构建内嵌网关）：
```
$ gateway.exe zcode-login quota --uid x --auth-dir <dir>
（打印启动横幅，永不退出）
```

**修复**：`bin/gateway.rs` 的 `main()` 第一句拦截子命令，**在任何 `-config` 解析之前** —— 否则未知子命令会落进「无 config 启动」路径并常驻，症状与缺陷本身一模一样。

**交付**：`qoder-login` 10 个子命令 + `zcode-login` 8 个子命令，**18/18 真机验证通过**。

### 2.2 排程器的两个注入点从未接线（P0）

`set_product_tasks_runner` / `set_billing_cycle_refresh_runner` 在 `bin/gateway.rs` 里**零命中** ⇒ 两者都是 `None`。

| 能力 | 状态 |
|---|---|
| Qoder 自动领取（20 分钟巡检那条路径） | **没丢** —— `multi_product_credit_patrol.rs:107` 独立实现 |
| Qoder 时点排程（10:00 / 21:00） | ❌ 丢了 |
| ZCode 分钟级领取（启动即跑 + 5 分钟轮询） | ❌ 完全没跑 |
| 手动 `/tasks/run?task=product_tasks` | ❌ 回 `skipped(... "该构建未接入产品任务执行体")` |
| `billingCycles` | ❌ 见第五节降级项 |

**用户可见后果**：`zcode-schedule-info.tsx:44-46` 在界面上**明确承诺**「网关启动时立刻跑一次，之后每 5 分钟检查一轮」，而实际什么都不跑 ⇒ 套餐权益随活动到期作废。

**修复**：新增 `products/product_tasks.rs`（Go `cmd/server/product_tasks.go` 198 行的对应物）+ 在 `bin/gateway.rs` 里接线 `set_product_tasks_runner` 与 `tokio::spawn(claim_sched.run(&cancel))`。

**真机验收**（隔离实例，端口 7910 / 7911）：

| 场景 | `POST /tasks/run?task=product_tasks` |
|---|---|
| 修复前 | `{"message":"该构建未接入产品任务执行体","ran":false,"skip":"unsupported"}` |
| 修复后（`multi_product=true`） | `{"message":"Qoder 领取 0 项（失败 0）；ZCode：没有可用凭证，跳过","ran":true}` |
| 修复后（`multi_product=false`） | 仍 `skipped` —— ★ 与 Go 门控一致，**这是正确行为** |

### 2.3 启动路径从不调 `pool.sync_to_dir`（P0，会复活已删账号）

生产 `state.json` 里有 **31** 条账号，而 `gateway_auths\` 只有 **27** 个凭证文件 ⇒ **4 个孤儿条目**。

`load_state()` 会为 state.json 的**每个**条目建一个**空 token 占位条目**；而 `healthy()` 与 `pick_inner()` **都不检查 `access_token` 是否为空**（只有 `probe_uids()` 检查）⇒ 孤儿条目会被正常选中，用空 token 发请求 ⇒ 必然 401。

**触发条件**：你在界面里删掉任一 WorkBuddy 账号（`commands.rs:142 delete_account` 只改账号库；`gateway.rs:961-970` 的清理循环只删 `workbuddy*` 文件）后，下一次网关启动就会把它**复活**成一个空 token 账号。

**修复**：`bin/gateway.rs:229-249` 用 `pool.sync_to_dir(&accounts)` 取代逐个 `add()`（对应 Go `main.go:206`）。★ 位置**必须在**多产品 `pool.add` **之前** —— `sync_to_dir` 的剔除是按 uid 全量比对的。

---

## 三、修掉一处**并发**缺陷：客户端断开时账号锁泄漏

Go 用 `defer r.Unlock(a.UID)`；Rust 版把解锁写在 `await` **之后** —— 而 **axum 在客户端断开时会丢弃 handler future** ⇒ 任何 `await` 之后的清理都被跳过 ⇒ 该账号**永久 busy**。

**真机复现**：30/30 次轮询、跨 150.4 秒，全部返回
```
400 该账号的任务正在执行中，请稍候
```

**修复**：改用 RAII 守卫（`AccountGuard`）镜像 Go 的 `defer`。**修复后：5.0 秒内返回 200。**

---

## 三点二、补上「计费周期统计」（界面上「本周期用量 / 下次重置」的数据源）

Go 的 `internal/usage/cycle.go`（380 行）在 Rust 重写时**整份漏掉**，于是
`/usage` 的 `billingCycles` **永远是空 map**，界面上「本周期用量 / 下次重置」永远没有数据
—— 而且**不报任何错**。它是**三段**，本轮把三段都补齐了：

| 段 | Go | 本轮 |
|---|---|---|
| ① 周期存储/投影 | `internal/usage/cycle.go` | 移植到 `usage.rs`（1316 → 2725 行，+30 测试） |
| ② 从上游取 `nextResetAt` | `internal/qoder/userstatus.go` | 新写 `userstatus.rs`（880 行，+17 测试） |
| ③ 巡检循环接线 | `cmd\server\billing_cycle.go` + `main.go:771` | 新写 `billing_cycle.rs`（815 行，+17 测试）+ `bin\gateway.rs` 接线 |

**三条硬语义逐字保留**：读路径**只投影、绝不回写**（回写会丢掉上一个周期的积分，且会自死锁）；
`maxCycleCatchUps = 2400` 防病态数据；`MaxPlausibleResetMs`（2100-01-01）合法性闸。

**真机实测**（真实 Qoder 账号的只读副本）：

```
计费周期刷新：处理 1 个 Qoder 账号，1 个周期有更新
```
`GET /usage` ⇒ `billingCycles` 从**空 map** 变为 `{"known": true, "cycleStartAtMs": …, "nextResetAtMs": …}`。

无 Qoder 账号时**如实说明**而不是静默：
`计费周期刷新未接线（池里没有 Qoder 账号或未启用多产品）：/usage 的 billingCycles 将为空`

★ **顺带修掉一个会静默失效的缺陷**：`serde_json` 用默认 features（无 `preserve_order`）
⇒ `Map` 是 `BTreeMap`，`json!({...})` 按键的**字典序**重排字段，破坏「字段顺序参与签名」
这条硬约束 —— **不报错**，只让上游签名校验失败。已改用 `#[derive(Serialize)] struct`
按声明序序列化，并有反证测试钉住。

★ **已知耦合**（照搬 Go，不是疏漏）：它搭的是**积分巡检**那条循环，
故 `credit_refresh_enabled=false` 时本刷新**也不跑**。

---

## 三点五、修掉「reqwest 偷偷走系统代理」

**形状**：reqwest 的 `ClientBuilder` 默认 `auto_sys_proxy = true`，`build()` 时会
`proxies.push(ProxyMatcher::system())`，在 Windows 上读的是**注册表**
（`HKCU\...\Internet Settings`）。而 Go 的 `http.ProxyFromEnvironment` **只读环境变量**
⇒ 「没配代理」在 Go 是**真直连**，在 reqwest 是**偷偷走系统代理**。

**为什么一直没暴露**：`Matcher::system()` 没有 `cfg` 门，它经 `hyper-util` 的
`client-proxy-system` feature 解析。而 `hyper-util` 在整个 workspace 里**只有一个版本**
（0.1.20），`src-tauri` 的 `tauri-plugin-updater` 拉 reqwest 0.13.4 时会打开该 feature，
Cargo 的 feature 统一**按包版本**做 ⇒ 我们自己的 reqwest 0.12.28 在 `--workspace`
构建里**也**拿到了系统代理能力：

```
cargo tree -p ai-gateway-router -e features  → 无 client-proxy-system
cargo tree --workspace          -e features  → 有 client-proxy-system
```

**症状**：本机开着 `127.0.0.1:7890` 的 mihomo 时，发往本机假上游的请求被代理重新发起 ——
代理是 Go 程序、补上自己的 `User-Agent: Go-http-client/1.1`；连不上时拿到代理的 **502**
而不是传输层错误。

**A/B 证据**（同一条命令 `cargo test --workspace --lib`）：

| 状态 | 结果 |
|---|---|
| 缺陷在（`None => {}`） | **2 FAILED** —— 断言打印出 `User-Agent: Go-http-client/1.1` 与 `502` |
| 修复后（`.no_proxy()`） | **1403 passed / 0 failed** |

**修复**：`build_proxy` 返回 `None` 时显式 `.no_proxy()`。

**新增 2 条回归测试**，并做了**反向验证**：把修复还原后新测试确实变红并打印出被注入的
UA —— 证明它真能抓到（第一版只断言「请求到达」，走不走代理都能过，是**假绿**，已重写）。

---

## 三点七、修好 webui 形态下 4 个「点了没反应」的按钮

**缺陷形状**（本项目**反复出现**的一类）：前端 `api.ts` 的 `ROUTES` 表里早就登记了路径，
而服务端**从来没有那条路由** ⇒ 浏览器形态下点了 404；而桌面端走 Tauri IPC
（`call()` 在 `isWebui()` 为 false 时用 `invoke`），**所以桌面端完全看不出问题**。

| 前端调用点 | 路径 | 此前表现 |
|---|---|---|
| `SettingsPage.tsx:145` | `POST /api/proxy/test-upstream` | 「测试上游」按钮无反应 |
| `SettingsPage.tsx:165` | `POST /api/proxy/probe-nodes` | 「探测节点」按钮无反应 |
| `sms-login-dialog.tsx:89` | `POST /api/gateway/sms/send` | 短信登录「发送验证码」404 |
| `sms-login-dialog.tsx:104` | `POST /api/gateway/sms/verify` | 短信登录「验证」404 |

★ 这**不是偶发**：`crates/ai-gateway-server/src/api/proxy_api.rs` 的文件头**已经记录过**
同一类缺陷（当时修的是 `/api/proxy/*` 与 `/api/tasks/*`）—— 本次是残留的 4 条。

**修复**：补上 4 条路由（业务逻辑与桌面端**同源**，只做「取参数 → 调用 → 转 JSON」，
不重写任何探测/登录逻辑）。

★ **新增回归闸门**：`api.rs::every_frontend_route_path_has_a_server_handler` ——
逐条断言 `api.ts` 的 `ROUTES` 里**每条路径**在服务端都有 `.route("<path>")` 注册。
反证已验证：删掉任一条 `.route(...)` ⇒ 测试 FAILED 并列出缺失路径。
同时把 `cargo test -p ai-gateway-server` 加进 `ci.yml` 与 `release.yml` ——
**这个 crate 此前从未在 CI 里跑过测试**，不加的话那条闸门等于没写。

★ **适用范围**：`ai-gateway-server` 经 **npm** 分发（`ai-gateway-win32-x64` 等平台包），
**不在** Windows 的 NSIS 安装包里 —— 故本次修复不影响 `AI Gateway_1.0.14_x64-setup.exe`。

---

## 四、实测：全链验收

| 项目 | 结果 |
|---|---|
| `cargo check -p ai-gateway-router --lib --offline` | **EXIT 0，0 warnings** |
| `cargo check -p ai-gateway-router --bin gateway --offline` | **EXIT 0，0 warnings** |
| `cargo test -p ai-gateway-router --lib --offline` | **1465 passed / 0 failed** |
| `cargo test --workspace --lib --offline` | **2619 passed / 0 failed**（13 + 1141 + 1465） |
| `cargo check --workspace --all-targets --profile test --offline` | **EXIT 0** |
| `cargo test -p ai-gateway-core --lib --offline` | **1141 passed / 0 failed / 4 ignored** |
| CLI 全矩阵（真机，18 子命令） | **18/18 通过** |
| `/tasks/run` 门控（真机，两个实例） | ✅ 见 2.2 |
| 锁泄漏（真机，前后对照） | ✅ 150.4s 全挂 → 5.0s 通过 |
| **包内实测**（从 `dist\` 的安装包解出的网关，非 `target\`） | ✅ 见 4.3 |

### 4.1 内嵌的确实是新网关（逐字节）

```
embedded/gateway.exe   13,089,792 B
SHA256 9BDA6AE9E4195C1BB903553A366A6A1778D21E43B4D9849E2EBBAB651B42869C
target/release/gateway.exe
SHA256 9BDA6AE9E4195C1BB903553A366A6A1778D21E43B4D9849E2EBBAB651B42869C
⇒ ★ 逐字节一致
```

### 4.2 CLI 退出码契约（★ 与 Go 逐字对齐，含两处**刻意的不对称**）

宿主判据（`qoder_login.rs:41-91`）：非零退出 ⇒ 把 **stderr** 当错误消息、**丢弃 stdout**；零退出 ⇒ 必须能把 stdout 解析成 JSON。

⇒ **业务失败必须返回 0**，原因放 stdout `{"status":"error","message":"…"}`；**参数错误返回 2** 且 stdout 必须为空。

真机实测（内嵌二进制，非单测）：

| 用例 | exit | stdout |
|---|---|---|
| `zcode-login poll --session "   "` | **1** | 空（stderr：`登录会话不存在或已过期（   ）。请重新发起登录`） |
| `zcode-login import --credential "   "` | **1** | 空（stderr：`凭证为空`） |
| `zcode-login quota --uid "   "` | **2** | 空（stderr：`缺少 --uid 参数`） |
| `zcode-login bogus-subcommand` | **2** | 空（stderr：`未知子命令 "bogus-subcommand"（应为 start / poll / …）`） |
| `zcode-login models`（凭证缺失） | **0** | `{"status":"error",…}` ← ★ Go 的**刻意分叉**，`models` 永不返回 1 |
| `qoder-login quota --uid nobody` | **0** | `{"message":"在 … 里找不到 uid=nobody 的凭证","status":"error"}` |

★ 前三条正是 Go `logincli.go` 里**同类检查写法不一致**的直接后果：`:351` poll `--session` 与 `:902` import `--credential` 是 `== ""`（**不 trim**，故空白串会走到业务层以 **1** 收场），而 `:643` quota / `:792` models / `:434` 是 `strings.TrimSpace`（trim，故空白串按缺参以 **2** 收场）。**这三条分界已逐条真机钉住。**

### 4.3 ★★ 交付包本身实测（不是 `target\` 里那份）

按 `AGENTS.md` 的规定流程**解包比对**（**绝不运行安装包** —— Tauri 的 NSIS 模板按**可执行文件名**杀进程，而主程序与所有者正在跑的实例同名）：

```
1) 7z 解包  dist\AI Gateway_1.0.14_x64-setup.exe
   ⇒ ai-gateway.exe  38,940,672 B
2) 从 build 产物解出内嵌 gz 并比对
   gateway_embed.gz → 13,090,816 B
   SHA256 B54300AC6713A42EE8C037BDCFF0B0A0440CE4382D15970B86BA5BDBF9C50C9C
   = crates/ai-gateway-core/embedded/gateway.exe（逐字节一致）
3) ★ 在**解包出的** ai-gateway.exe 里逐字节搜索该 gz
   ⇒ 命中 1 次，偏移 24,134,554
   ⇒ 交付包里携带的**就是**这次重建的 Rust 网关
```

然后把**从包里解出的**网关拷成独立名字、用独立端口/独立数据目录真跑：

| 验收项 | 结果 |
|---|---|
| 启动日志两条产品领取行 | ✅ 逐字对齐 Go |
| `POST /tasks/run?task=product_tasks` | ✅ `200 {"message":"Qoder 领取 0 项（失败 0）；ZCode：没有可用凭证，跳过","ran":true,"task":"product_tasks"}` |
| `GET /status` | ✅ `200 {"accounts":[],"cooling":0,"disabled":0,"healthy":0,"in_flight_full":0,"redis_mode":"noop","sticky_sessions":0,"total":0}` |
| CLI 参数错误 / 未知子命令 | ✅ 全 exit 2 + stdout 空 |

**交付包**：

```
dist\AI Gateway_1.0.14_x64-setup.exe   14,096,868 B   2026-10-01 13:44:53
SHA256 EFB23EFA2377E98CBA18D3BEC0B52290DDFDE071B3F18F69D12570E5458E96A5
内嵌网关 SHA256 12ACC8FB4DB92FD80B0CF76069946647716CE06DB12B5E3E0ED4BE04DD8C7A43
```

（取代早先四份：`13,438,311 B / 865F1ED1…` 含第三节三处接线缺陷；
`14,074,142 B / 737EC02C…` 含「三点五节」的系统代理缺陷；
`14,075,670 B / F98024DD…` 缺「三点二节」的计费周期；
`14,094,753 B / E301A102…` 缺 ZCode F-09 测试资产。）

★ 注：Rust release 构建**不确定**（同一份源码两次构建 SHA256 不同，实测确认）。
故网关指纹**只在同一次构建链内可比**；跨次构建请以「功能实测」为准。

---

## 五、已知问题与降级项（如实列出）

1. **`/usage` 的 `billingCycles` 全仓库零消费者** —— 键照常产出，但 `src/**`、`crates/**`、`src-tauri/**` 里**没有任何读取方**。裁定：**如实记为降级项，不修**。
2. **`set_billing_cycle_refresh_runner` 仍未接线** —— 上表第 1 条的接线点。与第 1 条同一决定。
3. **ZCode 真 JWT 路径未经端到端验证** —— `preview-plan` / `claim-plan` / `warm` 的**成功路径**没跑过（手上没有真实 ZCode JWT）。已验证的是：契约与退出码逐条正确、错误分支真实、`models` 真走上游（返回 `"source":"upstream"`）。凡涉及真 JWT 的**成功**路径属未验证。
4. **Qoder OAuth 的 `ready` 分支未在真机走到** —— 需要人工点浏览器授权。`start` 的会话落盘路径已验证。
5. **ZCode 验证码求解器子进程不走代理** —— 已知、**刻意不修**（与 1.0.13 同口径）：线上 `proxy_scope = {"cn":false,"intl":true}` ⇒ 国服流量本就不走代理，而阿里云验证码是**国内**服务。
6. **人机验证不会被绕过**（刻意如此）—— 遇到 `captcha_required` 请按提示在 ZCode 官方客户端里完成一次验证后重试。
7. **`redisstore` 刻意未移植** —— `/status` 的 `redis_mode` 恒为 `"noop"`。
8. **测试代码有 4 条无害警告** —— 全在 `#[cfg(test)]` 里（两条中文测试函数名触发的 `non_snake_case` 是本仓库既有惯例，另两条未使用项）。
9. **`11140` 的根因是「该账号在本设备上尚未完成登录」** —— 网关只能冷却它并自动切到别的号；要让这个号恢复，需在「账号管理」页**重新登录该账号**。

---

## 六、按你的决定**未做**的事

- **访客账号提交入口** —— 跳过，按「第 4 条按『已完成』收尾」结项（理由见 1.0.13 说明第七节）。
- **`git filter-repo`** —— 未要求，未做。

---

## 安装

- **Windows**：`AI Gateway_1.0.14_x64-setup.exe`（NSIS）
- 升级后**旧的 `gateway-*.exe` 会被自动清理**，无需手工处理
