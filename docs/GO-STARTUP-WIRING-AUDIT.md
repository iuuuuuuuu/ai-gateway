# Go 启动接线 → Rust 对照审计（P0 缺陷类的系统性排查）

> 生成于 2026-10-01。用途：**防止再犯**「实现搬过来了、接线丢了」这一类缺陷。
>
> 本会话里它已造成 **3 次真实 P0**（CLI 子命令、`set_product_tasks_runner`、
> `pool.sync_to_dir`）+ **1 次功能静默失效**（Qoder 计费周期三段缺两段）。
> 四次都满足同一形状，故做一次**穷尽对照**。

---

## 为什么这类缺陷特别危险

三项特征叠加 ⇒ 常规验收**抓不到**：

1. **实现是完整的** —— 单元测试全绿，代码审查也看不出问题
2. **接线点在 HTTP 面之外** —— 43 项 HTTP 验收一条都覆盖不到
3. **失败是静默的** —— 不报错、不崩溃，只是「这个功能永远不生效」

★ **可复用的判据**：把 Go `main.go` 的启动接线清单与 Rust `bin/gateway.rs` **逐条对差**。
不是「有没有实现」，而是「**有没有人调它**」。

---

## 对照表（Go `cmd/server/main.go` 全量）

| Go 行 | 接线 | Rust `bin/gateway.rs` | 裁定 |
|---|---|---|---|
| `:139` | `auth.LoadDir(cfg.AuthDir)` | `:233 auth::load_dir` | ✅ |
| `:150` | `p.SetStore(store)` | — | ⛔ **刻意不移植**（redisstore 已裁定不做） |
| `:154` | `p.SetModelPlatforms(...)` | `:128` | ✅ |
| `:177` | `cs.SetDir(captchaDir)` | `mod.rs:98` 内 `configure_captcha_from_host()` | ✅ **间接**（见下） |
| `:183/197` | `cs.SetExternalSolver(url, tok)` | 同上 | ✅ **间接** |
| `:205` | `p.RestoreFromSnapshot()` | — | ⛔ 同 `:150`（依赖 redisstore） |
| `:206` | `p.SyncToDir(auths)` | `:247` | ✅ **曾是 P0#2 第四处缺口** |
| `:235` | `p.SetBreaker(...)` | `:107` | ✅ |
| `:236` | `p.SetMaxInFlight(...)` | `:112` | ✅ |
| `:237` | `p.SetWeights(...)` | `:113` | ✅ |
| `:241` | `p.SetRotation(true)` | — | ✅ **等价**：由 `cfg.pool.rotation` 分支处理（只打日志，Rust 池无 rotation 概念） |
| `:271` | `sessRouter.LoadFromStore()` | — | ⛔ 同 `:150`（依赖 redisstore） |
| `:312` | `up.SetProxy(proxy)` | `:369-374` 经 `ClientConfig.proxy` | ✅ |
| `:314` | `up.SetProxyScope(cn, intl)` | `:375-382` + `config.proxy_scope` | ✅ |
| `:331` | `up.SetRPCTimeout(...)` | `:370 ClientConfig.timeout` | ✅ **等价**（`SetRPCTimeout` 只改 WorkBuddy 的 `HTTP`/`intlHTTP`，不碰 ZCode） |
| `:448/468` | Qoder `LoadDir` + `p.Add` | `:272-289` | ✅ |
| `:483/495` | ZCode `LoadDir` + `p.Add` | `:300-314` | ✅ |
| `:527/535` | `zc.SetProxy(...)` | `client.rs::apply_host_proxy`（`mod.rs:100` 内调） | ✅ **间接** |
| `:539` | `zd.SetAuthDir(zcodeDir)` | — | ✅ **不需要**：Rust 侧 `load_credentials(dir)` 是自由函数，无状态要告知 |
| `:572` | `sch.SetProductTasksRunner(...)` | `:604` | ✅ **曾是 P0#2 主缺口** |
| `:583` | `p.SetMultiProduct(true, 0.3)` | `:124` | ✅ |
| `:644` | `p.SetProductModelsFallback(...)` | `:154` | ✅ |
| `:645` | `p.SetProductModels(...)` | `:155` | ✅ |
| `:652` | `p.SetPlatformModels(...)` | `:162` | ✅ |
| `:676` | `p.SetPlatformModelsDisabled(...)` | `:187` / `:203` | ✅ |
| `:771` | `sch.SetBillingCycleRefreshRunner(...)` | `:646` | ✅ **本轮补**（N4 最后一公里） |
| `:751` | `go sch.Run(ctx)` | `:764-770` | ✅ |
| `:756` | `startProductTasks(ctx, claimSched)` | `:788-794` | ✅ **本轮补** |
| `:776` | `go sch.RunCreditRefreshLoop(...)` | `:771-780` | ✅ |
| `:815` | `startConfigWatch(...)` | `cfgwatch` 接线 | ✅ |
| `:825` | 关闭 goroutine | axum `with_graceful_shutdown` | ✅ |

---

## ★ 「间接接线」这一类要特别注意

`captcha` 与 `zcode proxy` 在 Go 里是 `main.go` **显式**调两行；
在 Rust 里它们藏在 `ZcodeUpstream::new()` 内部（`products/zcode/mod.rs:98,100`）。

**判据**：只要那个构造函数在启动路径上被调（`bin/gateway.rs:717`
`ZcodeUpstream::new(product_client.clone())`），接线就成立。
★ **但不能只看「有没有那个函数」** —— 必须确认**它在启动路径上被调用**，
否则就是「实现 + 内部调用都在，入口没人调」的变体（比 P0#2 更隐蔽）。

---

## 结论

除**刻意不移植的 redisstore 三项**（`SetStore` / `RestoreFromSnapshot` /
`LoadFromStore`）外，Go `main.go` 的启动接线在 Rust 侧**已全部对齐**。

★ 新增接线时请**同步更新本表** —— 文档与代码分叉比没有文档更糟。

---

## 附：同一缺陷类的另一半 —— 前端 ROUTES ↔ 服务端路由

网关侧对齐之后，我把同一判据（「实现有没有**调用方**」）用到**宿主 WebUI** 上，
又抓到 **4 条真实缺口**（2026-10-01 修复）：

| 前端调用点 | 路径 | 缺口的用户可见表现 |
|---|---|---|
| `SettingsPage.tsx:145` | `POST /api/proxy/test-upstream` | 设置页「测试上游」按钮点了无反应 |
| `SettingsPage.tsx:165` | `POST /api/proxy/probe-nodes` | 「探测节点」按钮点了无反应 |
| `sms-login-dialog.tsx:89` | `POST /api/gateway/sms/send` | 短信登录「发送验证码」404 |
| `sms-login-dialog.tsx:104` | `POST /api/gateway/sms/verify` | 短信登录「验证」404 |

**为什么之前没被发现**：`api.ts` 的 `call()`（`:415`）在**桌面端**走 Tauri IPC
（`invoke`），经 `ROUTES` 表只在 **webui** 形态下生效 ⇒ 桌面端测试完全覆盖不到。
而 `proxy_api.rs` 文件头**已经记录过**同一类缺陷（`/api/proxy/*` 与 `/api/tasks/*`），
说明这不是偶发 —— 是**系统性的漏登记**。

**已加回归测试**：`api.rs` 的 `every_frontend_route_path_has_a_server_handler`
（读 `src/lib/api.ts` 的 `ROUTES` 表 + 本文件，逐条断言有 `.route("<path>")` 注册）。
反证已验证：删掉任一条 `.route(...)` ⇒ 测试 FAILED 并列出缺失路径。

★ 该测试有**两个容易写错的点**（我都踩过，注释已写进代码）：
1. **不能只 `contains(path)`** —— 注释里也有同样的路径 ⇒ 「路由删了、注释还在」会假绿。
   必须匹配 `.route("<path>"` 这个**注册形态**。
2. **必须先把 CRLF 归一成 LF** —— 本文件是 CRLF，多行写法的匹配串含 `\n`，
   不归一就**永远匹配不上**（假红）。

## 方法学教训

★ **审计「定义 vs 注册」这类清单时，不要用
`Select-String -Pattern ... -Context 0,N` 抓多行块再正则抽取** ——
`-Context` 会在 N 行处**静默截断**。我据此误报过「10 个 Tauri 命令定义但未注册」。

**正确做法**：`[System.IO.File]::ReadAllLines()` 定位块的起止行，再在**完整行区间**上迭代。

### 同一处的第二次错（更值得记：判据本身太窄）

第一次修正后我用 `commands::(\w+)` 抽取，得 86 —— 仍然**错**：
该模式**只认 `commands` 这一个模块**，而 `generate_handler!` 里实际有 **6 个模块前缀**：

| 模块 | 注册数 |
|---|---|
| `commands` | 86 |
| `commands_apps` | 80 |
| `commands_proxy` | 12 |
| `oauth_loopback` | 6 |
| `captcha_webview` | 1 |
| `tauri::`（单实例插件初始化，非命令） | 1 |
| **合计** | **186（其中 185 条真实命令）** |

⇒ 我的第一个数字（76）和第二个（86）**都低估了 100 条以上**。

**正确的判据必须与「模块名」解耦**：用 `(\w+)::(\w+)` 抓**所有**模块前缀，
再按模块逐个对照「`^pub (?:async )?fn` 的定义集」与「注册集」。

**最终核对结果：`commands` 86=86、`commands_apps` 80=80、`commands_proxy` 12=12、
`oauth_loopback` 6=6、`captcha_webview` 1=1 —— 全部零缺口。**

★ 唯一需要注意的是**不要把「内部函数」误判成缺口**：
`captcha_webview::spawn_solver_server` 与 `warmup_prefetch` 虽然 `pub fn`，但
**不是** `#[tauri::command]`（它们收 `AppHandle`，由 `lib.rs:217/226` 在启动时直接调用）。
判据应是「该函数是否有 `#[tauri::command]` 标注」，
而不是「它是不是 `pub fn`」。


