# Trae / 豆包 与 TraeWorkAssistant 功能对齐报告

对照对象：[`smart-open/TraeWorkAssistant`](https://github.com/smart-open/TraeWorkAssistant)（参考版本 **v3.5.8**）。

本报告记录**已落地的对齐项**、**有意不对齐项**及其理由，以及**过程中发现的十个非显而易见缺陷**。
后续如需复核某条功能为什么长这样，先查这里。

---

## 一、总体结论

参考项目与本仓库的产品定位不同，因此对齐的目标是**能力面**而不是逐行移植：

| 维度 | 参考项目 | 本仓库 | 对齐方式 |
| --- | --- | --- | --- |
| 架构 | Tauri 单体命令层 + 进程内调度线程 | Rust workspace（core / router / server / tauri 四层） | 业务逻辑落 `ai-gateway-core`，Tauri 与 HTTP server 都是**薄宿主** |
| 宿主 | 仅桌面 | 桌面 + WebUI 双宿主 | 每个能力都要有 Tauri 命令**与** HTTP 路由两条通道 |
| 存储 | Stronghold/DPAPI + SQLite `store/` | JSON + `atomic_write` | 不引入新存储引擎（见第二节） |
| 调度 | 进程内线程 | schtasks 为主 + 进程内调度为辅 | **两条并存**，都走同一个 `cli_task` |
| 前端 | 自研组件 | shadcn + Rhea 主题 | 一律复用既有组件，不照搬参考实现的样式 |

对齐后的验证基线（全部通过）：

- `cargo test -p ai-gateway-core --lib` → **772 passed / 0 failed / 1 ignored**（连跑 10 次稳定）
- `cargo test -p ai-gateway-router --lib` → **284 passed**
- `cargo test -p ai-gateway-server` → **3 passed**
- `cargo check -p ai-gateway --all-targets`（src-tauri）→ exit 0
- `cargo check -p ai-gateway-server --all-targets` → exit 0
- `npx tsc --noEmit` → exit 0
- `npm run build` → ✓ built（仅有既有的 >500 kB chunk 提示）

> 用例数会随其他并行开发变动；上表是本次对齐收尾时的实测值。

---

## 二、已落地对齐项

### Trae

| 能力 | 落地位置 | 说明 |
| --- | --- | --- |
| OAuth 授权码登录（PKCE + state） | `crates/ai-gateway-core/src/modules/trae_oauth.rs`、`src-tauri/src/oauth_loopback.rs` | 含 17388 回环监听、5 分钟空闲自停、CSRF/PKCE 校验、HTML 转义 |
| OAuth 手动回调兜底 | `oauth_submit_callback` | 端口被占用 / WebUI 模式下的唯一可用路径 |
| JWT 自动续期（ExchangeToken） | `crates/ai-gateway-core/src/modules/trae_refresh.rs` | 懒刷新阈值 48h、60s 冷却、拒绝与瞬时故障分流 |
| 积分总额 / 明细 / 套餐身份 | `crates/ai-gateway-core/src/modules/trae_credits.rs` | 三接口分别取套餐、总额、积分包；部分失败如实上报 |
| 积分每日快照与趋势 | `apps_ops::trae_credits_stats` | 按天补齐、缺天为 `null`、充值不计入消耗 |
| 账号分组（Trae / 豆包分域） | `crates/ai-gateway-core/src/modules/account_groups.rs` | 分组只影响筛选，删除分组不删账号 |
| 账号编辑（昵称 / JWT / refresh_token） | `apps_ops::trae_update_account` | 换账号的 JWT 会被**拒绝**，避免两个账号记录混在一起 |
| 完整 JWT 查看与解析预览 | `trae_account_jwt` / `trae_jwt_parse` | 列表始终脱敏，明文仅在弹窗按需展示 |
| 账号导出 / 导入 | `apps_ops::trae_export_accounts` / `trae_import_accounts` | 带 `kind` 标记防串库；同 uid 覆盖；无 JWT 记录跳过 |
| 签到失败重试（30s / 90s） | `apps_ops::trae_checkin_run` + `RETRY_DELAYS` | 只重试瞬时故障；永久失效与业务失败不重试 |
| 签到成功率趋势 | `crates/ai-gateway-core/src/modules/trae_checkin_results.rs` | 按日落库、同 uid 覆盖为最终态、90 天裁剪；`skip` 不计入 |
| 签到范围选择 | `TraePage.tsx` 勾选框 | 不选 = 全部 |
| 签到实时进度 | `checkin-progress` 事件 + `checkin:*` 阶段 | 重试等待期间必须有反馈 |
| 积分消耗明细（官方口径） | `crates/ai-gateway-core/src/modules/trae_usage_history.rs` | 直连会话级用量接口，拿**真实扣费**而非余额差值；含模型排行 |
| 全部冷却一键清除 | `trae_clear_all_cooldowns` | 含只计数未落冷却的残留记录 |
| 快照体积 / 文件数 | `apps_ops::list_snapshots` | 递归统计，不跟随符号链接 |
| 打开客户端（零副作用） | `apps_ops::app_launch` + `switcher::Action::LaunchOnly` | 不切账号、不备份、不关进程 |
| 客户端版本与配置来源 | `apps_ops::detect_app_version` | 探测不到就是 `null`，不回退成本应用版本 |
| 「当前登录」标记过期提示 | `switcher::resolve_current_uid` + `markerStale` | 标记与现场比时间戳；只信标记会让徽章标错账号 |
| 抓包日志查看（列表 / 详情 / 概况 / 清理） | `crates/ai-gateway-core/src/modules/proxy_logs.rs`、`src/components/proxy-logs-panel.tsx` | 按 `proxy_req_*` 分块解析；关键字 / 时间区间 / 分页；SSE 模型与 token 摘要 |

### 豆包

| 能力 | 落地位置 | 说明 |
| --- | --- | --- |
| 到期分层（未知 / 充裕 / 临期 / 过期） | `doubao_account.rs::expiry_tier` | 7 天阈值；临期提醒让用户「还来得及救」 |
| 快照元信息（体积 / 文件数 / 修改时间） | `apps_ops::doubao_list_accounts`、`doubao_snapshot_meta` | 列表直出，不需要额外请求 |
| 孤儿快照行 | `apps_ops::doubao_list_accounts` | 账号没了但快照还在时仍可见、可删 |
| 一键以账号打开 | `apps_ops::doubao_open_as_account` | 无快照时**禁用**（不是静默失败） |
| 删除账号（可选连快照一起删） | `apps_ops::doubao_delete_account` | 用 Dialog 承载复选框，替代 `window.confirm` |
| 运维健康史（14 天趋势 + 7 天健康卡） | `crates/ai-gateway-core/src/modules/doubao_health.rs` | 保活 / 续期 / 额度三类事件 |
| 25 天未保活告警 | `DoubaoPage.tsx` | 会话约 30 天到期，留 5 天补救窗口 |
| 续期失败自动回退保活 | `apps_ops::doubao_renew_with` | HTTP 路子走不通时拉起客户端，结果如实写回响应 |
| 快照 IndexedDB 开关 | `doubao_settings` / `doubao_set_setting` | 白名单键，不暴露整个设置文件 |
| UID 探测（三来源） | `apps_ops::doubao_detect_uid` | 抓包 → `Local State` → `current_account.txt` |

### 通用

| 能力 | 落地位置 |
| --- | --- |
| 应用内调度（应用运行期间到点执行） | `apps_ops::run_in_app_due_tasks` + `lib.rs` 循环 |
| 计划任务 5 项（含 Trae 积分快照、豆包 HTTP 续期） | `scheduler::TaskKind` |
| 路径安全统一守卫 | `config::ensure_uid_safe` |
| 抓包日志查看（设置页） | `src/components/proxy-logs-panel.tsx` |
| 演示模式覆盖守卫（漏一条即构建失败） | `scripts/check-demo-coverage.mjs` + `npm run build` |
| 跨平台 demo / 截图构建脚本 | `.env.demo`、`.env.pages-demo`、`scripts/dev-screenshot.ps1` |

---

## 三、过程中发现并修复的非显而易见缺陷

### 1. `config::utc_iso()` 的时间格式与 RFC3339 不兼容

`config::utc_iso()` 产出 `%Y-%m-%dT%H-%M-%SZ` —— **时间部分用连字符而不是冒号**。

后果：任何按 RFC3339 解析该字段的代码会**静默丢弃本仓库自己写下的每一条记录**。
`doubao_health` 的趋势图与健康卡最初全空、且不报任何错误，根因就在这里。

修复：`doubao_health::parse_iso_ts` 同时接受本仓库格式与 RFC3339，并加了回归用例
`认得本仓库自己的时间格式`。

### 2. `TaskKind::DoubaoRenew` 的命名陷阱

`scheduler::TaskKind::DoubaoRenew` 的 `cli_key` 是 **`doubao-keepalive`**、启动器名是
`doubao_renew`，但它实际执行的是**保活**而不是 HTTP 续期。

这两个标识符**已经注册在用户机器上**，改名会让既有计划任务失联，因此保留原样，
另加 `DoubaoRenewHttp`（`doubao-renew` / `doubao_renew_http`）承载真正的 HTTP 续期。

已加守护用例 `豆包两个续期类任务的_cli_键不得对调` 与
`计划任务名与启动器名互不冲突`，防止后续维护时对调。

### 3. 全账号冷却时会写下一条「空签到日」

`trae_checkin_results::record_today` 原本无条件建出当天的日期条目。当所有账号都在
冷却中（该轮一个请求都没打），传入的是**空迭代器**，于是当天留下一条
`accounts: {}` 的空记录，趋势接口算出 `total = 0`。

后果有两层：前端按比例分段时 `p.ok / p.total` 得到 **NaN**，柱状图渲染出 NaN 宽度；
即便不崩，图上也会多出一根零高度柱子，读起来像「那天全军覆没」，实际是那天没跑。

修复：空输入直接返回，不建条目；前端再按 `total > 0` 过滤一遍，兼容旧版本可能已经
写下的空记录。回归用例 `全跳过时不建出空日期条目` 与 `当天的累计不会被空输入清掉`。

### 4. 积分统计只有一个数据点时 `change` 会算成 0

`trae_credits_stats` 的区间变化量原本由「首末两天相减」得出。只有一天数据时首末是
同一行，相减得 `0` —— 而 `0` 在界面上读作「没变化」，真相是「无从得知」。

修复：至少两天数据才算变化量，否则返回 `null`。回归用例
`积分统计按天补齐且缺天为_null` 就是这个 bug 暴露出来的。

### 5. 并行测试下「动作闸门」用例会随机失败（既有隐患）

`switcher::ActionGate` 是**进程级**全局锁，而 cargo 默认在同一个测试二进制里
并行跑用例。原有的 `动作闸门互斥且释放后可再次获取` 直接断言「第一把锁立刻拿到」，
于是只要同一时刻有别的用例正在 `run_action`（任何调用它的用例都会持有这把锁），
它就会失败 —— 而且失败信息与被测逻辑毫无关系。

原先没暴露只是**碰巧**没有其它用例在同一窗口内调用 `run_action`；本轮新增
「仅拉起客户端不改动任何快照槽」后立刻撞上。

第一版修法只等「第一次获取」，结果释放之后的那句断言仍然会和别的用例抢 —— 
实测在连续验证中又红了一次。最终修法是**两次获取都等到空闲为止**：
「持有期间别人拿不到」这段（真正的被测语义）保持为无竞争断言，
而「释放后能再拿到」由「别人这时能拿到」本身来证明，不再依赖调度顺序。
连跑 10 次全量用例稳定通过。

### 6. 「当前登录」标记会过期却被无条件信任

`current_account.txt` 只在本应用执行「切换 / 保存登录态」时写入。用户完全可能
**直接打开客户端换一个账号登录** —— 此时现场已经是新账号，标记还停在上一个，
界面据此把「当前登录」徽章打在错的账号上，用户按它判断「现在用的是哪个号」
就会得出相反结论。

参考项目用 `current_cloud_uid_hybrid` 处理同类问题（标记与证据比时间戳取新的）。
本仓库按同样的思路补齐：写标记时同时写 `current_account.meta.json`
（`switchedAtMs`），读的时候与现场关键文件的改动时间比较，
现场更新超过 2 秒就返回 `markerStale: true`，界面弹出「可能已过期」提示。

两个关键取舍：**uid 始终来自标记**（现场文件里读不出 uid，只能证明标记旧了，
不能给出新值）；**只比较承载登录态的那几个文件**（`storage.json` / `state.vscdb` /
`Local State` / `Cookies`），整个目录取最大改动时间会被缓存与日志顶高，
把刚做完的切换误判成过期。容差 2 秒同理：切换流程里写标记与客户端落盘几乎同时。

### 7. 参考项目的 SSE 摘要提取恒为空（静默失效）

参考项目 `extract_sse_field` 的写法是先把行 `trim()`、再用**带两空格缩进**的前缀
`"  model: "` 去 `strip_prefix`。缩进在 `trim()` 时已经被去掉，前缀里的空格于是
永远匹配不上 —— 结果是它的抓包日志列表**恒不显示**模型名与 token 用量，
而且不报任何错误（只是那两列一直空着）。

本仓库照搬功能时把这个 bug 一起发现了：移植时先 `trim()` 整行、
再用**不带缩进**的前缀匹配。守护用例 `sse_字段带缩进也必须能取到`
（缩进 / 无缩进两种写法都要能取到）。

### 8. 演示模式下有 12 条只读命令声明了却没有实现

`screenshot-demo.ts` 的 `screenshotDemoResponse` 是 `switch` + `default: throw`。
`DEMO_READ_COMMANDS` 里声明了命令、但没有对应 `case`，调用即抛
「演示模式缺少只读数据」—— 截图 / 演示模式点开 Trae 或豆包页就是一片报错。

其中 `get_codebuddy_cn_ide_status` 是**既有问题**，其余 11 条是本轮新增 Trae / 豆包
只读命令时漏加 `case`（声明与实现是两处，容易只改一处）。
已补齐全部 12 条，并逐条对齐真实返回形态（含缺数据的天、空分组、从未跑过的任务、
401 日志条目等分支，让截图能看到真实状态差异而不是清一色 happy path）。

> 这类「两处清单必须同步」的结构没有编译期保护，本轮是靠脚本比对
> `DEMO_READ_COMMANDS` 与 `screenshotDemoResponse` 的 `case` 集合发现的。
> 当前 44 条声明全部有实现，且已加 `scripts/check-demo-coverage.mjs` 并接进
> `npm run build`，再漏一条会直接构建失败。

### 9. 页面「挂载即读」的命令没进演示模式，整块数据空白

接第 8 条继续排查时发现更严重的一类：命令**根本没被声明**，但页面一挂载就调用它。
`screenshotDemoResponse` 抛错 → `Promise.all` 整体 reject → **整页没有任何数据**。

受影响的是 `app_env_check`（Trae 页与豆包页都在 `Promise.all` 里读它，一旦失败连
账号列表都渲染不出来）、`trae_checkin_trends`、`trae_usage_history`、
`proxy_config` / `proxy_status` / `proxy_cert_status` / `task_status`。

第 8 条是「声明了没实现」，这条是「实现了但没声明」—— 两个方向都会让演示模式出问题，
所以校验脚本按**声明 ⊆ 实现**这一条边来查。同时给这些只读项补齐了假数据，
并刻意包含分支数据（一个从未注册的计划任务、一个全是「已签到」的签到日、
一个有失败项的官方用量账号）。

另外这轮还修掉了这类问题的**根因**：`screenshotDemoResponse` 的返回值在
`api.ts` 里被 `as T` 强转，假数据形状写错（字段名拼错、少字段、类型不符）
`tsc` 一声不吭。已给新增的假数据函数加显式返回类型注解
（从 `api.ts` 做 `import type` 导入，编译期擦除，不引入运行时循环引用）。
实测把 `payIdentity` 写成 `payIdentityTypo` 会直接报
`TS2561: 'payIdentityTypo' does not exist in type 'TraeCreditDetail'`。

### 10. 演示 / 截图相关的 npm script 在 Windows 上无法执行

`package.json` 里的四条脚本用的是 POSIX 内联变量写法：

```
"dev:demo": "VITE_DEMO_MODE=1 vite"
"build:demo": "tsc && VITE_DEMO_MODE=1 VITE_PAGES_DEMO=1 vite build"
"tauri:dev:screenshot": "AI_GATEWAY_SCREENSHOT_DEMO=1 VITE_DEMO_MODE=1 tauri dev"
```

`VAR=value cmd` 在 Windows 的 cmd / PowerShell 下不成立，`build:demo` 直接报
`'VITE_DEMO_MODE' 不是内部或外部命令`。仓库运行环境是 Windows
（`AGENTS.md` Shell Policy），也就是**演示构建在这台机器上从来没成功过**。

改法（不引入 `cross-env` 依赖）：

- `vite build --mode pages-demo` + `.env.pages-demo`（`VITE_DEMO_MODE=1` /
  `VITE_PAGES_DEMO=1`）。`--mode` 是跨平台的，`.env.<mode>` 只影响
  `import.meta.env`，不碰 shell。
- `base` 原本读 `process.env.VITE_PAGES_DEMO`，而 Vite **不会**把 `.env.<mode>`
  灌进 `process.env`，所以同步改成配置里的 `loadEnv(mode, ...)` ——
  否则会得到一个「环境变量生效了但 `base` 没换」的半成品构建。
- `tauri:dev:screenshot` 需要给 **Rust 进程**设环境变量（`is_screenshot_demo()`），
  这一侧 Vite 帮不上，改为 `scripts/dev-screenshot.ps1` 显式设置后调用 `tauri dev`。

验证：`npm run build` 与 `npm run build:demo` 均通过，`build:demo` 产物入口为
`/ai-gateway/assets/...`（确认 `base` 生效）。

---

## 四、有意不对齐项

以下能力参考项目有、本仓库**不移植**，逐条记录理由。

### 架构约束导致的

| 项 | 参考实现位置 | 不对齐理由 |
| --- | --- | --- |
| Stronghold / DPAPI 凭据库 | 参考的凭据加密层 | 需要 Tauri 依赖；`ai-gateway-core` 必须保持无 Tauri 依赖（架构约定）。改用文件权限 + 明确提示。若要引入，需要先单独决策。 |
| SQLite `store/` | 参考的持久化层 | 本仓库全线是 JSON + `atomic_write`。混入第二种存储引擎会让备份/迁移逻辑分叉。 |
| 进程内调度线程（唯一调度） | 参考的 scheduler | 本仓库以 schtasks 为主（应用没开也能跑）。改为纯进程内会让「关机期间的任务」全部丢失。现在是两者并存。 |

### 上游抓包常量导致的

| 项 | 不对齐理由 |
| --- | --- |
| OAuth client / app 常量外置为 `conf/oauth_client.json` | 这些是**版本化的上游抓包值**，外置成用户可改的配置意味着上游一改就没人知道该填什么。改为内置常量 + 注释标明来源与抓包日期。 |
| `icube_auth` ECDSA / `tc_decrypt` 字节表 | 属于上游私有协议的逆向产物，逐字节搬运维护成本极高且无文档。本仓库用等价的设备指纹推导（`trae_device::derive_device`）已达到同样效果。 |
| 手写 leveldb / SST / snappy 解析器（约 300 行） | 解析的是客户端**内部**存储格式，随客户端改版即失效。本仓库走 Chromium 多 Profile 快照（`SCHEMA_VERSION=1`），是官方支持的布局。 |
| guest session 12 小时魔数阈值 | 该阈值来自参考作者的抓包观察，不是上游文档承诺。写死会随时间失效且无从校验。 |
| `schemaVersion=0` 旧快照兼容分支 | 本仓库的快照格式是第一版发布，不存在 v0 存量数据。加一个永远走不到的兼容分支只会增加维护面。 |
| 硬编码 `%LOCALAPPDATA%\Doubao\User Data` | 本仓库通过 `app_profile` 解析布局，且支持用户手动指定安装路径。硬编码会在非默认安装时直接失效。 |

### 产品决策导致的

| 项 | 不对齐理由 |
| --- | --- |
| 邀请链接（`commands/misc.rs:11`） | 那是参考作者的个人邀请码。移植等于替第三方做推广，与本产品无关。 |
| updater endpoint | 更新源必须指向本仓库自己的发布通道，用别人的端点会把用户更新到无关产物上。 |
| 捐赠 / 品牌素材 | 与功能无关。 |

### 已经等价、无需重复实现的

| 项 | 本仓库既有实现 |
| --- | --- |
| icube 15 路径布局 + WAL/SHM | `switcher/icube.rs` |
| 机器指纹 6 层 + `>=4` 阈值 | `switcher/machine.rs` |
| 设备指纹 uid 确定性派生 | `trae_device::derive_device` |
| 签到 precheck → claim → 分类 → 冷却 | `trae_checkin.rs`（8 类 `ErrorKind`） |
| 豆包多 Profile 快照 + `.bak` 单代回滚 | `switcher/chromium.rs` |
| 豆包两阶段探活 / 续期 | `doubao_session.rs` |
| 豆包额度严格 + 宽松双解析 | `doubao_quota.rs` |
| 豆包对话备份 / 恢复 / IM 导出 | `doubao_page` 相关命令 |
| 快照管理（列表 / 备份 / 恢复 / 删除） | `apps_ops::list_snapshots` / `delete_snapshot` + switcher 动作 |
| 应用设置读写（patch 合并语义） | `get_app_settings` / `save_app_settings` + `config::set_app_setting` |
| 开机自启开关 | `get_launch_at_login_enabled` / `set_launch_at_login_enabled` |
| uid 探测链（抓包 → `Local State` → 落盘记录） | `apps_ops::doubao_detect_uid` 三来源 |

### 参考项目有、但属于其桥接层历史包袱的

| 项 | 不对齐理由 |
| --- | --- |
| `current_account.txt` 作为 uid **来源之一** | 参考项目用它接收 PowerShell 桥写回的「当前账号」（UTF8 带 BOM 的文本文件）。本仓库没有 PS 桥，uid 由抓包 / `Local State` / 落盘记录三处直接得出，加一个没有写入方的第四来源只会让探测链多一个永远为空的入口。（注：这个文件在本仓库另有用途 —— 标记「当前登录账号」，其**过期判定**已按参考的混合思路补齐，见第三节第 6 条。） |
| `credits_daily_list` 与 `usage_history_fetch` 并存 | 参考项目保留两套消耗口径（快照差值 + 官方接口）是为了兼容旧数据。本仓库同时提供两者但**明确分工**：`trae_credits_stats` 是余额净变化，`trae_usage_history` 是真实扣费，界面上并排展示并注明差异，而不是让两个数字互相矛盾。 |
| `logs_query` / `logs_clear` 统一日志查询 | 参考项目把所有日志汇到一个查询接口。本仓库的**抓包日志**已经补齐查看（列条目 / 详情 / 概况 / 清理，见 `proxy_logs`）；`logs/proxy.log`、`logs/switcher.log`、`logs/gateway.log` 这几份**操作日志**仍各自独立 —— 它们是逐行文本、没有分块结构，且各自的脱敏规则不同，合并查询要么放弃差异、要么写一层与现有实现重复的聚合。 |
| `profile::profile_format_size` | 参考项目把它做成一条命令。本仓库的 `formatBytes` 在 `src/components/proxy-logs-panel.tsx` 与 `TraePage.tsx` 各有一份纯展示实现 —— 这是纯前端格式化，让 Rust 为了界面显示往返一次没有收益。 |
| `misc::read_text_file` / `misc::write_text_file` | 参考项目的通用文件读写命令，服务于它的导入/导出流程。本仓库的导入走文件选择器 + 后端路径校验（`trae_import_accounts` / `apps_ops` 内的路径守卫），导出走 blob 下载；加一对无差别读写的通用命令会绕开现有的路径白名单，属于纯扩权。 |
| `accounts_backfill_dc_id_for` 回填 dc_id | 参考项目用它给旧账号记录补齐「账户中心 uid」字段。本仓库的 `trae_discover` 已明确区分两个 uid 空间（`iCubeAuthInfo://icube-dc:` 是账户中心空间，**不可入库**），没有需要回填的历史字段 —— 这是参考项目自身数据演进的产物。 |

### 命令名对照（参考侧 185 条 → 本仓库 160 条的对应关系）

参考项目与本仓库的命名约定不同（本仓库一律 `trae_*` / `doubao_*` / `apps_*` 前缀），
所以逐条比对的是**能力**而不是名字。下表只列「名字不同、需要说明归属」的那些，
其余按能力一一对应：

| 参考项目命令 | 本仓库对应 | 说明 |
| --- | --- | --- |
| `env::open_trae_app` / `open_trae_cn_app` / `open_doubao_app` / `open_codebuddy_app` / `open_workbuddy_app` | `app_launch(target_app)` | 参考是 5 条独立命令，本仓库合并为一条带 `targetApp` 参数的命令（5 个入口 → 1 个实现，行为一致）。 |
| `env::app_locate` / `env::env_check` / `env_check_trae_cn` | `app_env_check(target_app)` | 同上，按 `TargetApp` 参数化。 |
| `env::open_trae_website` | **未移植** | 打开官网是纯外链，界面上用普通链接即可，不需要一条后端命令。 |
| `misc::autostart_status` / `autostart_set` | `get_launch_at_login_enabled` / `set_launch_at_login_enabled` | 同一能力，本仓库沿用既有命名（底层同样是 `tauri-plugin-autostart`）。 || `misc::task_register` / `task_status` / `task_unregister` | `task_register` / `task_status` / `task_unregister` | 同名同语义；本仓库的 `TaskKind` 有 5 种（多了积分快照与凭证续期巡检）。 |
| `misc::jwt_parse` | `trae_jwt_parse` | 同能力，本仓库加 `trae_` 前缀。 |
| `misc::device_reset` | `trae_device_info(userId, reset)` 与 `switch_action` 的 `ResetDeviceIds` 动作 | 本仓库把设备重置并入设备信息命令与切换动作，不单列。 |
| `misc::settings_get` / `settings_set` | `get_app_settings` / `save_app_settings` | 同能力，且本仓库是 patch 合并语义（见「已经等价」表）。 |
| `misc::invite_link` | **不移植** | 参考项目的邀请返利功能，与本仓库产品定位无关。 |
| `accounts::account_add_manual` / `account_update` / `account_delete` | `trae_add_account` / `trae_update_account` / `trae_delete_account` | 一一对应。 |
| `accounts::accounts_list` | `trae_list_accounts`（另有通用侧 `get_accounts`） | Trae 与 WorkBuddy 侧账号分开维护，各自有清单命令。 |
| `accounts::account_get_jwt` / `refresh_jwt` | `trae_account_jwt` / `trae_refresh_account`（批量 `trae_refresh_all`） | 一一对应，本仓库另有整批刷新。 |
| `accounts::accounts_import` / `accounts_import_preview` | `trae_import_accounts` / `trae_preview_import` | 一一对应。 |
| `accounts::accounts_export_raw` | `trae_export_accounts`（另有通用侧 `export_accounts` / `export_accounts_to_path`） | 本仓库导出即原始凭证（明文 JSON），不再区分「原始 / 加工」两种形态。 |
| `accounts::cooldown_clear` / `cooldown_clear_all` | `trae_clear_cooldown` / `trae_clear_all_cooldowns` | 一一对应。 |
| `accounts::fetch_remaining_credits` / `refresh_remaining_credits` / `fetch_credit_detail` | `trae_credit_detail` / `trae_refresh_pay_status` / `trae_credits_stats` | 本仓库按「明细 / 刷新 / 统计」组织，口径划分见下条。 |
| `accounts::credits_daily_list` | `trae_credits_history` + `trae_credits_snapshot` | 本仓库按「历史（读缓存）/ 快照（采样）」拆分。 |
| `accounts::group_create` / `group_update` / `group_delete` / `group_move` / `groups_list` | `group_create` / `group_update` / `group_delete` / `group_move` / `groups_list` | 同名同语义（`account_groups.rs`）。 |
| `switch::switch_account` | `switch_action`（动作 `Switch`；通用侧另有 `switch_account`） | 本仓库把切换统一成一个 `switch_action`（7 种动作：`Switch` / `SaveCurrentLogin` / `BackupCurrent` / `RestoreOnly` / `ResetDeviceIds` / `KeepAlive` / `LaunchOnly`），Trae/豆包按 `targetApp` 区分。 |
| `switch::save_current_login` | `switch_action` 的 `SaveCurrentLogin` 动作 | 并入统一动作，不再单列命令。 |
| `switch::reset_device_ids` | `switch_action` 的 `ResetDeviceIds` 动作 + `trae_device_info(userId, reset)` | 同上。 |
| `profile::profile_list` / `backup` / `restore` / `delete` | `list_snapshots` / 切换动作内备份 / `switch_action` 恢复 / `delete_snapshot` | 快照管理见「已经等价」表。 |
| `proxy::proxy_start` / `status` / `stop` | `proxy_start` / `proxy_status` / `proxy_stop` | 同名同语义。 |
| `cert::cert_install` / `cert_status` | `proxy_cert_generate` / `proxy_cert_status` | 本仓库按「生成 / 查询」命名（安装动作在界面上引导用户点确认，不用命令代劳）。 |
| `checkin::checkin_start` / `checkin_trends` | `trae_checkin_run` / `trae_checkin_trends` | 同名同语义。 |
| `doubao::*`（24 条） | `doubao_*`（同名或近名） | 逐条对应；差异项见上文各表。 |
| `oauth::*` / `oauth_loopback::*` | `oauth_login_url` / `oauth_*` + `src-tauri/src/oauth_loopback.rs` | 能力一致（PKCE + state + 回环回调）。 |
| `usage_history::usage_history_fetch` | `trae_usage_history` | 同能力，见「桥接层历史包袱」表。 |
| `wb_config::*` / `api_server::*` / `workbuddy_*` / `ccswitch::*` / `updater::*` | 不在本次对齐范围 | 这些是 WorkBuddy / 网关配置 / 更新器侧的命令，与「Trae + 豆包 功能对齐」的目标无关（网关与 WorkBuddy 侧本仓库自有实现）。 |

### 明确不属于参考能力的（不要当缺口）

- Trae 国际版 / SG 站点（参考项目的 backlog F-71，标为远期）
- 多实例（参考项目的 backlog F-67，标为待调研）

---

## 五、移植过程中避开的两个陷阱
1. **额度百分比只能取自 `ParsedQuota::windows`**（camelCase 的 `usedPercent`），
   **不能**取 `items` —— `items` 是宽松兜底路径，只保证等级/名称，不含百分比。
   从那里取会恒为 `None`，字段变成永远为空的死值。

2. **会话预检必须读 JSON 凭证池**（`doubao_account.rs`），
   **不能**解密快照里的 Cookies —— 客户端会把 Cookies 重新加密，解出来的不是可用凭证。

---

## 六、验证方式

```powershell
# Rust
$env:RUSTC_WRAPPER=""; $env:SCCACHE_DISABLE="1"
cargo test -p ai-gateway-core --lib
cargo test -p ai-gateway-router --lib
cargo test -p ai-gateway-server
cargo check -p ai-gateway --all-targets

# 前端
npx tsc --noEmit
npm run build
```

一致性自检（本报告所述的通道完整性）：

- `src/lib/api.ts` 的 `ROUTES` **152** 条，每条都能在
  `crates/ai-gateway-server/src/api.rs` 找到对应路由（0 条缺失）。
- 除 `switch_progress`（webui 轮询专用）外，`ROUTES` 里的每个命令名都在
  `src-tauri/src/lib.rs` 的 `generate_handler!` 中注册。
- `DEMO_READ_COMMANDS` 的 **47** 条命令在 `screenshotDemoResponse` 里都有对应
  `case`（0 条会抛「演示模式缺少只读数据」），该不变式由
  `scripts/check-demo-coverage.mjs` 守，并已接进 `npm run build`（漏一条即构建失败）。
- `npm run build` 与 `npm run build:demo` 均通过；后者产物入口为
  `/ai-gateway/assets/...`（GitHub Pages 子路径）。

本轮新增的能力在两端都已接通：

| 能力 | Tauri 命令 | HTTP 路由 |
| --- | --- | --- |
| 签到成功率趋势 | `trae_checkin_trends` | `GET /api/trae/checkin/trends` |
| 积分消耗历史 | `trae_usage_history` | `GET /api/trae/usage-history` |
| 积分快照采样 | `trae_credits_snapshot` | `POST /api/trae/credits/snapshot` |
| 打开客户端 | `app_launch` | `POST /api/apps/launch` |
| 抓包日志列表 | `proxy_logs_list` | `GET /api/proxy/logs` |
| 抓包日志详情 | `proxy_log_detail` | `GET /api/proxy/logs/detail` |
| 抓包日志概况 | `proxy_logs_overview` | `GET /api/proxy/logs/overview` |
| 抓包日志清理 | `proxy_logs_clear` | `POST /api/proxy/logs/clear` |
