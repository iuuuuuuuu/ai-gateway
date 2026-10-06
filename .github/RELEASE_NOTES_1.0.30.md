## AI Gateway 1.0.30 · 正式版

> 本版修完你上次列的四件事：**① 切手动模式/改白名单会把 Trae 网关凭证删光（而且不自愈）；② 后台签到/任务跑完界面不刷新；③ 后台拉客户端会弹窗打断你，现在默认藏起来，设置页有开关；④ ZCode `glm-5.3-flash` 对话中途重试报「error decoding response body」/「Internal Network Failure」**。
>
> 四件事全部**真机实测**：前两件用安装包里解出来的二进制跑 A/B/C 对照，第三件在隔离实例里点勾选框对照磁盘文件，第四件用假上游把三种断流形态各跑一遍（48/48 断言全过）。
>
> 顺带修掉一个**只有后台任务才会撞到**的记账缺陷：豆包保活失败也会被记成「今天已完成」，导致当天不再重试（详见第四节）。

---

## 一、你报的四个问题，结果

| 你的原话 | 结论 |
| --- | --- |
| 「切手动模式/改模型白名单会删掉 Trae 网关凭证（凭证目录被清空且不自愈）」 | **已定案并修复**：Trae 的凭证导出把**WorkBuddy 的 uid 勾选名单**当成了过滤条件，两套 uid 空间交集恒空 ⇒ 清理循环把 `trae-*.json` 全删；且切回 balance 不会自愈。现在恒不过滤 |
| 「后台签到成功或者任务执行完成当然要刷新状态了」 | **已修复**：调度循环跑完会 `emit background-task-done`，前端收到后按 kind 刷新对应页面 |
| 「在后台启停客户端我希望可以隐藏那个窗口去运行，如果不能，就加个开关」 | **已修复**：能隐藏。后台保活拉起客户端后把它的窗口藏起来（默认开），设置页「后台运行」有开关 |
| 「`zcode:glm-5.3-flash` 对话中途重试报 `upstream stream ended before the completion event: error decoding response body` 与 `Internal Network Failure`」 | **已定案并修复**：上游流被截断时，旧二进制把 reqwest 的 `Display` 原文（`error decoding response body`）直接透传给你。现在按成因归类改写，且**不惩罚账号** |

### 1.1 Trae 凭证被清空（最要命的那个）

先说结论：**不是你操作错了，是导出逻辑用错了 uid 空间**。

Trae 的凭证导出链路上有一个 `active_uids()`：

```rust
// 修复前（1.0.23 起）
fn active_uids() -> Option<Vec<String>> {
    if matches!(gateway::gateway_mode(), GatewayMode::Manual) {
        Some(gateway::manual_uids())   // ← 这是 WorkBuddy 的 uid
    } else {
        None
    }
}
```

`gateway::manual_uids()` 返回的是**账号管理页勾选的 WorkBuddy uid**（形如 `1883919207380040`），
而 Trae 的 uid 是 Trae 自己的 `user_id`。两套 uid 空间**交集恒为空**，于是：

1. `picked` 为空 ⇒ `export_accounts_to_dir` 的清理循环认定「这些凭证都不该在」，把 `trae-*.json` **全部删掉**；
2. 网关的账号池是**扫描凭证目录**建立的，宿主那份 `trae_accounts.json` 它根本不读 ⇒ 池当场变空，Trae 请求全部失败；
3. 而且**不会自愈**：切回 balance 只走 `start_gateway` 里那一次直调，`export_if_changed` 的指纹没被更新，重启之前一直不补回来。

修法就一行 —— `active_uids()` 恒返回 `None`（不过滤）。Trae 侧本来也没有「手动模式」这个界面概念（网关页的勾选列表只列 WorkBuddy 账号），过滤在这里没有任何意义，只有破坏性。

> 注意：`disabled` 标记**依旧不过滤**。禁用只表示「别把流量路由给它」，标记由 `account.no_route` 下传，账号本身仍要导出 —— 否则网关池里没有它，而池是唯一能遍历到它的地方，表现为「禁用后任务全停」。这条语义没变。

### 1.2 后台跑完不刷新

原因很直白：前端那两个轮询 hook（`useWorkbuddyStatusRefresh` 等）**只在「文档可见 && 主窗口可见 && 窗口聚焦」时才跑**。
你把窗口收进托盘/最小化，后台签到成功了，界面就一直停在旧状态 —— 不是数据没写，是**没人去读**。

现在桌面端调度循环每跑完一项就发一条事件：

```rust
let _ = app.emit("background-task-done", serde_json::json!({
    "kind": kind, "label": label, "ok": ok, "exitCode": code,
    "scheduledAt": at, "at": checkedAt,
}));
```

前端 `useBackgroundTaskRefresh()`（新增 `src/lib/use-background-task-refresh.ts`）订阅它，先刷账号状态，再在 window 上派发 `dsh:background-task-done`；
Trae / 豆包页面各自监听该 DOM 事件并按前缀过滤刷新（`trae*` / `doubao*`）。

### 1.3 后台拉客户端会弹窗

豆包保活的动作是「拉起客户端 → 等 8 秒 → 关掉」，窗口会直接弹到你桌面上打断你正在做的事。

现在：**只有无人值守的动作才隐藏**，也就是 `Action::KeepAlive`；其余动作（打开客户端 / 切换账号 / 保存登录态）是你刚亲手点的，窗口正是你要看的，藏起来等于把功能做没了。

```rust
pub fn hides_client_window(self) -> bool {
    matches!(self, Action::KeepAlive)
}
```

隐藏用 `ShowWindow(hwnd, SW_HIDE)`（枚举该进程的顶层窗口），并且加了设置项开关：

| 项 | 值 |
| --- | --- |
| 配置键 | `client_hide_window`（`app_settings.json`） |
| 默认值 | `true`（默认隐藏） |
| 界面位置 | 设置 → 后台运行 → 「后台运行时隐藏客户端窗口」 |

> 桩实验对照（`test-bin/reqC-hide/`）：隐藏开着时日志有 `[switch] [start] info 已在后台隐藏 豆包 的 1 个窗口`，关掉时**没有**这一行；两条路径的保活结果都是 `{"message":{"Ok":"保活完成"},"ok":true}` ⇒ 隐藏不影响功能。

### 1.4 ZCode 断流（`error decoding response body`）

旧二进制在流中途被截断时，会把 **reqwest 的错误 Display 原文**塞进给客户端看的 message：

```
upstream stream ended before the completion event: error decoding response body
```

这句话既没有主语（哪个上游）、也没有成因（连接被掐 / 正常结束但没给结束帧 / 卡住不动），而且和「账号有没有问题」混在一起。

现在按**成因**归类后再给你看，并且新增了一条硬规矩：`REQWEST_KIND_NOISE`（`error sending request` / `request or response body error` / `error decoding response body`）这三句原文**一律不许出现在客户端可见文案里**。

三种形态实测的客户端可见正文（逐字）：

| 形态 | 客户端看到的 message |
| --- | --- |
| 上游发完内容帧就掐断 socket | `upstream stream ended before the completion event: 上游流中断：error reading a body from connection ← unexpected EOF during chunk size line` |
| 上游正常结束响应体但没发结束帧 | `upstream stream ended before a completion event; this turn may be incomplete` |
| 上游卡住不动（空闲超时） | `upstream stream ended before the completion event: 上游流中断：流中空闲超时（上游在配置的空闲时限内没有吐出任何数据）` |

另外三件一起做到的事：

1. 错误帧是**协议级**的（`{"error":{"code":"upstream_stream_truncated","type":"upstream_error"}}`），并且**恰好一个** `data: [DONE]` 收尾 —— 你的客户端不会挂在那儿等；
2. **传输层截断不罚账号**：三种形态下账号状态都是 `err_total=0`、无 `last_err`、`disabled=false`、`cool_kind=0`；
3. 「上游在流中下发错误帧」这类日志不会误报（那是上游主动发的错误帧，和传输截断是两回事）。

至于你看到的 **`Internal Network Failure`**：那是**上游自己**在流里下发的错误帧内容，网关只做识别 / 记日志 / 计入账号健康度，**从不改写它** —— 这句话本身就是上游的原话。

---

## 二、真机实测记录

### 2.1 用安装包里的二进制跑（不是工作区产物）

```
dist\AI Gateway_1.0.30_x64-setup.exe   14,545,639 字节
SHA256 7307D32175869D5EBB32300FA520F5B03915327DF94E7B79BE1E5C6BD1F82BD0
```

7-Zip 解包后拿到 `ai-gateway.exe`（40,178,176 B）与内嵌网关（13,629,440 B），
反解出的网关与 `crates/ai-gateway-core/embedded/gateway.exe` **逐字节一致**。
下面的实测全部用**包内解出来的这两份**，隔离实例（identifier 已打补丁），不碰你正在跑的那份。

### 2.2 Trae 凭证（需求 A）

本机没有 Trae 真账号，所以这一项是**源码级定案 + 3 条新单测**：

- `手动模式不得把_WorkBuddy_的勾选名单当成_Trae_的过滤条件`
- `网关切到手动模式后_Trae_凭证仍在`（走真实入口 `export_accounts_to_gateway()`，配 `mode=manual` + WorkBuddy uid 后断言凭证文件仍在）
- `导出参数只在同一_uid_空间内筛账号`

### 2.3 后台刷新（需求 B）：三臂对照

| 臂 | 程序 | 账号池 | 结果 |
| --- | --- | --- | --- |
| A | **修复前**的 1.0.30 | 非空 | `[调度] 豆包会话保活 计划 09:10 ⇒ 成功（退出码 0）`，`in_app_schedule.json` **记了当天**，`last_keepalive_at` 被写成一个**失败的**时间戳 ⇒ 缺陷复现 |
| B | **修复后** | 非空 | `[调度] 豆包会话保活 计划 09:10 ⇒ 失败，将在下一轮重试（退出码 1）`，当天**不记账**，`last_keepalive_at` 仍为 `null` |
| C | **修复后** | **空** | `{"ok":true,"reason":"没有豆包账号","skipped":true}`，退出码 0（跳过），**不写健康事件** |

事件链路的实测（上一轮 1.0.29 的探针，本版沿用）：

```
rawEvents(原生)  doubao-keepalive ok:true  +  trae-checkin ok:true
domEvents        同上两条（前端确实收到了）
cmds.afterFirstDomEvent(+10s)
  ["trae_list_accounts","app_env_check","trae_checkin_status","trae_checkin_trends"]
```
⇒ 事件之后**确实发生了刷新调用**，不是「发了没人听」。

### 2.4 隐藏窗口开关（需求 C）

在隔离实例上点设置页那个勾选框，对照磁盘文件：

| 步骤 | DOM | `app_settings.json` |
| --- | --- | --- |
| 初始 | `checked=true` | `{ "client_hide_window": true }` |
| 点一下 | `checked=false` | `{ "client_hide_window": false }` |
| 再点回来 | `checked=true` | `{ "client_hide_window": true }` |
| 刷新页面 | `checked=true`（重新读盘） | `{ "client_hide_window": true }` |

### 2.5 ZCode 断流（需求 D）：48/48 全过

假上游把三种断流形态各演一遍，用**包内解出的网关**跑，逐项断言：

```
mode=trunc  16 项 PASS
mode=eof    16 项 PASS
mode=stall  16 项 PASS
SUMMARY pass=48 fail=0
```

每种形态都断言了：客户端拿到 **HTTP 200**（不是 5xx）、正文**不含** `error decoding response body`、**恰好一个** `data: [DONE]`、有协议级错误帧、网关 stderr 无 panic、账号**未被惩罚**。

---

## 三、会不会影响我的历史记录 / 现有功能？

不会：

1. Trae 那条改的是**过滤条件**（恒不过滤），只会「多导出」不会「少导出」，凭证文件只会变多不会变少；
2. 后台刷新是**纯新增**一条事件 + 一个前端 hook，不动任何既有请求路径；
3. 隐藏窗口只在 `Action::KeepAlive` 生效，你手动点的动作行为**完全不变**；
4. 网关二进制与上一版**逐字节相同**（差异只有编译期时间戳和调试 GUID，`.text` 段哈希一致）⇒ 转发行为零变化；
5. 全量单测：`cargo test --workspace` = **3011 passed / 0 failed / 6 ignored**（1.0.29 是 3007，本次新增 4 条）。

---

## 四、顺手修掉的记账缺陷（你上一条报的现象的另一半）

后台任务有个约定（写在代码注释里）：「只有成功才记账：失败要留给下一轮重试，否则一次网络抖动就等于当天不再尝试」。
但 `doubao-keepalive` 这条 CLI 分支违背了它：

```rust
// 修复前
let outcome = switcher::run_action(args, &switcher::LogSink);
let _ = doubao_account::set_last_keepalive(&config::utc_iso());  // ← 失败也写
let _ = doubao_health::record_keepalive(outcome.is_ok(), ...);
Ok(json!({ "ok": true, "message": outcome }))                    // ← 恒返 Ok
```

恒返 `Ok` ⇒ 退出码 0 ⇒ 调度认定「今天跑完了」⇒ **当天不再重试**；`set_last_keepalive` 无条件写 ⇒ 界面上「上次保活」会显示一个**失败的**时间戳。
你在你自己机器上已经撞到过：`doubao_health.json` 里 `ok:false,"summary":"保活失败"` 与 `in_app_schedule.json` 里同一天的记录**并存**。

现在：失败 ⇒ 退出码非零 ⇒ 5 分钟后重试；成功才刷新时间戳。另外补了**空账号池短路** —— 池子为空时直接 `{"ok":true,"skipped":true,"reason":"没有豆包账号"}`，
不写健康事件（否则调度每 5 分钟重试一个永远不会变的条件，一天 288 条足以把 400 条上限的健康史冲干净）。

---

## 五、安装提示

- 请先**从托盘退出**正在运行的 AI Gateway：安装脚本按进程名结束进程。
- 同版本号不覆盖，本版是 **1.0.30**；你实机当前是 **1.0.21**，直接装 1.0.30 即可。
  **装完这一个版本，上面四件事会一起消失** —— 你跳过的 1.0.22 到 1.0.29 里的修复，都在 1.0.30 里面。
- 装好后网关会自己换成本版本内嵌的那一份（1.0.26 起的能力），不需要手工删文件。
- 默认会把后台拉起的客户端窗口藏起来；想看着它跑，去 **设置 → 后台运行** 关掉那个勾选框。
- 想看调度有没有在跑：打开 `%USERPROFILE%\.wb-switch` 下的桌面端日志，搜「[调度]」即可。

---

## 六、交付物核验（可自行复核）

```
dist\AI Gateway_1.0.30_x64-setup.exe   14,545,639 字节
  SHA256  7307D32175869D5EBB32300FA520F5B03915327DF94E7B79BE1E5C6BD1F82BD0
dist\AI Gateway_1.0.30_x64-setup.exe.sig   424 字节
  SHA256  AA5EC776288085F7E8916602237BA0B22E3A6A17E7FAC1C157377FD5ECD25AAF
```

包内主程序与工作区构建产物只差 3 个字节（偏移 33403520-33403522，是打包器写入的 bundle 类型标记 `BUNDLE_TYPE_VAR_UNK` → `BUNDLE_TYPE_VAR_NSS`），
反解出的内嵌网关与 `crates/ai-gateway-core/embedded/gateway.exe` **逐字节一致**。
完整核验记录见 `test-bin/DELIVERY-1.0.30.md`。
