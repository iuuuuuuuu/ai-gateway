## AI Gateway 1.0.26 · 正式版

> 本版三件事：**① 修掉一个你看得见的严重缺陷**（点平台标签后画面不换）、**② 把豆包账号卡也接进统一任务区**、**③ 让网关自己换新**（治 m17014 的 `stream ended before the completion event`）。
> 1.0.25 的页面状态缓存、1.0.24 的启动卡顿修复、1.0.23 的 ⑪⑫⑬ 全部保留，本版没有回退任何东西。

---

## 一、你报的问题，结果

| 你的原话 | 结论 |
| --- | --- |
| 「页面切换保持状态缓存，不要重置」 | 1.0.25 做到了「不卸载」，但**做错了「不显示」**。根因见下，本版已修好 |
| 「重试延迟：500毫秒 / 失败原因：upstream stream ended before the completion event: error decoding response body」 | **本版治本**：网关每次启动都会比对「端口上正在跑的网关是不是本版本释放的」，不是就换成本版本内嵌的那个；子进程日志不再丢弃，落到 `gateway_data/gateway.log` |
| 「如果不是我点了一个账号发现任务都没执行我都不知道」 | 豆包账号卡补上统一任务区（本账号粒度），与 WorkBuddy/Qoder/ZCode/Trae 一致 |

### 1.1 严重缺陷：点了 Qoder，画面还是 WorkBuddy（1.0.25 引入）

这不是错觉，是**真机上抓到的、可稳定复现的**缺陷。我用隔离实例截图取证：

| 现象 | 证据 |
| --- | --- |
| 「Qoder」标签已高亮选中，但画面主体仍是 WorkBuddy | 截图 `uitest/shot-qoder.png`：标题「账号管理」、三级标签（账号/积分统计/Token 统计）、「账号 27」全在 |
| 外层 WorkBuddy 面板 `data-state=inactive` 却 `hidden=false` | 探针实测：`w=701, h=4471` 仍然可见；Qoder 面板被顶到 `top=4733` |
| 切回 WorkBuddy，Qoder 面板同样不隐藏 | 双向都坏 |

**根因（Radix 源码逐字）**：

`// node_modules/@radix-ui/react-tabs/dist/index.mjs`
`jsx(Presence, { present: forceMount || isSelected, children: ({ present }) => jsx(Primitive.div, {`
`  "data-state": isSelected ? "active" : "inactive",`
`  hidden: !present,`
`  children: present && children }) });`

`forceMount` 的语义是「**永远 present**」，不是「常驻但隐藏」。所以 `hidden = !present` 恒为 `false` —— keep-alive 的面板全部同时可见、纵向堆叠。1.0.25 引入 keep-alive 时把「不卸载」误当成「会隐藏」，缺陷就是这样进去的。

**修法**（`src/pages/AccountHubPage.tsx`，4 处）：

`// 外层平台面板`
`- <TabsContent key={t.key} value={t.key} forceMount={visited.has(t.key) ? true : undefined}>`
`+ <TabsContent key={t.key} value={t.key} forceMount={visited.has(t.key) ? true : undefined}`
`+              hidden={active !== t.key}>`

`// 三级面板（账号 / 积分统计 / Token 统计）同理`
`+ hidden={picked !== "accounts"}   // 或 "credits" / "tokens"`

**真机验证**（隔离实例，DOM + 截图双重）：

| 断言 | 结果 |
| --- | --- |
| 切到 Qoder 后 WorkBuddy 面板 `hidden=true` / `display=none` / `w=0` | **PASS** |
| Qoder 面板 `hidden=false` / `display=block` / `w=701` | **PASS** |
| 截图里只剩当前平台的内容（WorkBuddy 的 27 张卡彻底消失） | **PASS** |

---

## 二、豆包账号卡接入统一任务区（m02670 强约束）

你要求「所有平台的账号卡片统一显示任务数与执行结果、可重跑」。豆包是最后一个没接的：

| 任务 | 状态判定 |
| --- | --- |
| 凭证续期（本账号） | 无 `session_id` ⇒ `blocked`；今天已续 ⇒ `done`；否则 `ready` |
| 额度巡检（本账号） | 同 UTC 日已巡检 ⇒ `done`；否则 `ready` |
| 会话保活（全部账号） | 恒为 `detail`（它是全账号动作，不是单账号任务） |

- 卡片右上角 `⋯` 菜单与其它平台**同一套 DOM 契约**：`data-slot="product-task-item"` / `data-task` / `data-state` / `disabled`，分组标题「本账号任务{后缀}」，图标 done⇒绿勾 / blocked⇒警告 / detail⇒信息 / ready⇒播放。
- 末尾 `任务记录 · 消耗明细` ⇒ 打开该账号的记录弹窗。
- 后端新增两个**单账号粒度**命令：`doubao_renew_one(userId)` / `doubao_quota_one(userId)`（对应 `run_renewal_one` / `run_batch_one`，5 个退出路径全部留痕）。
- ZCode 账号卡同批补上 `zcode-warm`（激活并领取套餐）/ `zcode-refresh`（刷新额度与模型）。

---

## 三、网关自己换新（治 m17014）

**你报的**：「重试延迟：500毫秒 / 失败原因：upstream stream ended before the completion event: error decoding response body」。

**根因**：宿主程序启动时会「认领」端口上已在运行的网关进程（比如上次退出时留下的旧版）。旧版 router 的流式处理有缺陷，于是你一直踩在旧二进制上；而旧进程的 stderr 被丢进 `Stdio::null()`，所以出了问题连日志都没有。

**修法**（`crates/ai-gateway-core/src/modules/gateway.rs`）：

1. 认领前先比对：占用端口的进程文件名 `!=` 本版本内嵌网关名 ⇒ 判定「不是本版本释放的」，停掉它、换成本版本内嵌的。
   `[gateway] 端口 7874 上的网关不是本版本释放的，正在换成本版本内嵌的网关`
2. **来源闸门**：只有 `embedded` 来源才比文件名。`AI_GATEWAY_ROUTER_BIN`（手工指定）和「同目录自备 gateway.exe」的文件名本来就不带指纹，硬比会把每次启动都判成「需要换新」并反复重启 —— 比版本落后更糟。
3. 子进程 stdout/stderr 落 `gateway_data/gateway.log`（4 MB 轮转，保留 `.1`）。
4. 读占用者要跑 netstat + tasklist + powershell（数百毫秒）⇒ 挪进 `spawn_blocking`，不占异步 worker。

**真机复现（隔离实例）**：

| 步骤 | 结果 |
| --- | --- |
| 端口 7874 上先坐一个 1.0.21 的旧网关 | `HOLDER_BEFORE=gateway-4e52fa-ecd7924c953073e8|pid=58580` |
| 启动 1.0.26 宿主 | `SWAPPED_AT_SEC=4` |
| 换新后占用者 | `HOLDER_AFTER=gateway-4e8729-6ca281c408920fd4|pid=58916` |
| 旧进程 | `OLD_ROUTER_ALIVE_AFTER=False`（已被停掉） |
| `gateway.log` | **生成，10,184 B，UTF-8 正常** |
| 旧指纹文件 | `BIN_AFTER` 里已消失 |

---

## 四、本版改动面

| 文件 | 变更 |
| --- | --- |
| `crates/ai-gateway-core/src/modules/gateway.rs` | +190（换新判定 / 来源闸门 / 日志落盘 / spawn_blocking / 3 个单测） |
| `crates/ai-gateway-core/src/modules/doubao_session.rs` | +370（`run_renewal_one` / 留痕） |
| `crates/ai-gateway-core/src/modules/doubao_quota.rs` | +140（`run_batch_one` / 留痕） |
| `crates/ai-gateway-core/src/modules/gateway_embed.rs` | +31（`current_exe_name` 单测） |
| `src/pages/AccountHubPage.tsx` | +89（4 处 `hidden` + 乐观选中 + `?view` 保真） |
| `src/pages/DoubaoPage.tsx` | +244（统一任务区） |
| `src/pages/ZcodePage.tsx` | +62（`zcode-warm` / `zcode-refresh`） |
| `src/lib/api.ts` / `src-tauri/src/commands_apps.rs` / `src-tauri/src/lib.rs` | +40 / +13 / +2（两个单账号命令） |

合计 14 个文件，+1063 / −126。

---

## 五、验证记录

| 项目 | 结果 |
| --- | --- |
| `cargo check --workspace` | EXIT=0 |
| `cargo test` | **2988 passed / 0 failed / 5 ignored**（15 个套件） |
| `tsc --noEmit` | EXIT=0 |
| `vite build` | EXIT=0 |
| 真机断言（隔离实例，组 1–10） | **50 pass / 0 fail** |
| 网关换新真机复现 | PASS（4 秒内换新 + `gateway.log` 生成 10,184 B） |
| 安装包解包核验 | 包内主程序与工作区产物只差 3 字节（tauri bundle 标记）；内嵌网关与 `embedded/gateway.exe` **逐字节相同** |

---

## 六、升级提示

- 安装包 `AI Gateway_1.0.26_x64-setup.exe`（14,583,796 B）。
- NSIS 会**按进程名结束正在运行的 AI Gateway**，所以请先从托盘退出旧版本，再运行安装包。
- 你当前机器上装的是 **1.0.21**；1.0.25 与 1.0.26 都还没装。直接装 1.0.26 即可，中间版本不需要补装。
