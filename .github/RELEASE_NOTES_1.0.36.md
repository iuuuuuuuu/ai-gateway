# AI Gateway 1.0.36

**主题：把「重启 / 保活」这两件会打断用户的事，从「到点就干」改成「确认安全才干」。**

1.0.35 把网关日志补齐了时间戳、把流收尾凭据与 `/status.in_flight` 透了出来 ——
那是**观测**。1.0.36 用上了这些观测：既然现在能读到「此刻有几个请求在途」，
重启就不该再把正在写的回答拦腰截断。

---

## 一、重启前排空在途请求（修「回答写到一半断了」）

### 现象与根因

网关是宿主（GUI / server）的**子进程**。宿主调 `stop_gateway()` 时直接终止它，
正在服务的 SSE 流被拦腰截断 —— 客户端看到「回答写到一半断了 / 本轮运行失败」。

A 族断流（528 条）的根因排查过程中定位到：**每日维护窗口（本地 00:00:45）**
那两条 A 就是这个形状 —— 那个时刻正好有一次自动同步触发的网关重启。

### 修法

`restart_gateway_serialized()` 在 `stop_gateway()` **之前**先读
`/status.in_flight`，非 0 就每 250ms 轮询、最多等 30 秒；归零即停。

- **不等待的出口**：旧版网关（≤1.0.34）没有 `in_flight` 字段 ⇒ 按「不知道」处理、
  不等待并留日志。**绝不把「不知道」当成 0**（那会让优雅重启在最不该放的时刻放行，
  比不等待更糟）。
- **超时出口**：30 秒仍非 0 就照常重启并留日志 —— 无限等会让「账号变更生效」
  这类操作彻底卡死，那比偶尔打断一条长回答更糟。

### 实测（真链路，非单测）

隔离实例 `test-bin\inst-1036-drain`：宿主探针走**真实** `start_gateway()` 起网关，
并发发起一条 4.2 秒的慢流（假上游每 700ms 一帧），2 秒后调
`restart_gateway_serialized()`：

```text
[1] start_gateway ... 就绪: port=7863 is_running=true
[2] 流已发起 2s，/status.in_flight = Some(1)
[3] restart_gateway_serialized ...
    restart elapsed_ms=4218 ok=true
[4] 流：用时 4254 ms  saw_done=true  note=clean EOF after [DONE]
[5] 判定：排空生效 = true
[gateway] 重启前排空：当前 1 个请求在途，等待其结束（最多 30s）
[gateway] 重启前排空完成：在途请求已归零（等待 2352ms）
```

**关键读数**：客户端拿到完整 `[DONE]`（`saw_done=true`），而重启耗时
4218ms ≈ 那条流的剩余时长 —— 说明它确实等完了，而不是「碰巧流也刚好结束」。

---

## 二、保活兜底：整点漏了就补，不再静默丢一天

### 现象与根因

token 保活只在配置的整点（默认 **22:00**）触发。而网关是独立子进程：

```text
22:00 到了 → 网关没在跑（应用关了 / 刚重启 / 维护窗口）
           → 当天保活静默消失 → 下次机会 24 小时后
```

保活要防的正是「官方服务端清理闲置 refresh 会话」——闲置数天后刷新返回
**12153 invalid_grant**（Session doesn't have required client），账号被迫重新登录。
「用户每天 23:00 才开电脑」这类作息会让保活**永远不跑**。

### 修法：两个判据，各管一件事

| 判据 | 管什么 | 默认 |
|---|---|---|
| `schedule.keepalive_hours` | **节奏**：正常运行时的每日整点 | `[22]` |
| `schedule.keepalive_max_age_hours`（新） | **兜底**：距上次**成功**刷新超过它就在启动时补跑 | `24` |

- 网关把每次保活**成功**的时刻持久化到 `state.json` 的 `keepalive_at`（新增字段）。
- 启动时按「距上次成功是否 ≥ N 小时」筛出超期账号补跑；**未超期的一个请求都不发**。
- `keepalive_max_age_hours = 0` ⇒ 关闭兜底（逐字退回旧行为）；负数归一到 0。
- 补跑在**后台 spawn**：首次升级后所有账号的 `keepalive_at` 都是 0（旧 state.json
  没这个字段），27 个账号同步跑会让端口 30 秒不监听 —— 而补跑只改账号凭证，
  与监听无关，没有理由阻塞。

宿主侧的 WorkBuddy 保活循环同步改口径：原来比自然日（`date_str`），
现在按「距上次成功刷新的时长」判（`keepalive_days` 天，默认 1）。

### 实测

隔离实例 `test-bin\inst-1036-drain`（`AI_GATEWAY_HOME` 指向独立目录）：

1. `state.json` 里 `keepalive_at: 0`（= 从未成功过）→ 启动日志
   `[keepalive] 1 个账号距上次成功刷新已 ≥24h（或从未成功），开始补跑` →
   `补跑完成：1 个账号已刷新`；`state.json` 里 `keepalive_at` 写入真实时刻
   （实测 age=37s）。
2. 紧接着重启一次（`keepalive_at` 已是 37 秒前）→ **日志里一条 keepalive 都没有**，
   证明「未超期一个请求都不发」。
3. 端口绑定不再被补跑阻塞（启动日志里 `listening` 与 `开始补跑` 同毫秒）。

---

## 三、依赖告警处置（3 条）

| # | 包 | 生态 | 严重度 | 处置 |
|---|---|---|---|---|
| 1 | `source-map-js` | npm（dev-only） | high | **已修**：`1.2.1 → 1.2.2`（补丁版本 1.2.2） |
| 2 | `rustls` | rust | medium | **已修**：`0.23.43 → 0.23.45`（补丁版本 0.23.45） |
| 3 | `glib` | rust（**仅 Linux 目标**） | medium | **不适用**（见下，附可复核硬证据） |

### `glib` 为什么修不了

告警范围是 `>= 0.15.0, < 0.20.0`，即需要 **0.20.0 及以上**才算修好。而：

- `Cargo.lock` 里的 `glib 0.18.5` 来自 `gtk 0.18.2`，而 `gtk 0.18.2` 又来自
  `tauri 2.11.5`（经 `muda` / `tray-icon 0.24.2` / `libappindicator 0.9.0`）。
- `cargo update -p glib --precise 0.20.12` 直接报错：
  `failed to select a version for the requirement glib = "^0.18" ... required by package gtk v0.18.2`。
- 上游 `gtk` crate 的版本线止于 **0.18.x**（crates.io 上没有 `gtk 0.20`）；
  `tray-icon` 0.24.2 → 0.26.1 **每一个版本都仍写 `gtk ^0.18`**。
- **该依赖只存在于 Linux 目标**：`cargo tree -i glib --target x86_64-pc-windows-msvc`
  报 `nothing to print`；本产品发布的 Windows 安装包不链接 `glib`。

⇒ 这条告警要等 Tauri 上游把 `gtk` 升到 0.20 才能消，不在本项目可控范围。
本仓库在 Windows 上发布，实际暴露面为零。

---

## 四、可复核产物

- `crates\ai-gateway-core\examples\drain_probe.rs` —— 真链路排空探针
  （`start_gateway` → 慢流 → `restart_gateway_serialized` → 读耗时与 `[DONE]`）。
- `test-bin\inst-1036-drain\` —— 隔离实例（`AI_GATEWAY_HOME`），含慢流假上游与
  三阶段日志（`gw.log` / `gw2.log` / `gw4.log`）。
- 回归测试：`restart_drains_in_flight_before_stopping`、
  `drain_treats_missing_in_flight_as_unknown_not_safe`、
  `keepalive_staleness_boundaries`、`keepalive_max_age_negative_clamps_to_disabled`、
  `keepalive_due_closed_form`。

测试基线：**workspace 3056 passed / 0 failed / 7 ignored**（15 个 suite）。

---

## 五、配置新增项

```json
{
  "schedule": {
    "keepalive_hours": [22],
    "keepalive_max_age_hours": 24
  }
}
```

`keepalive_max_age_hours` 由宿主自动写入（默认 24），无需手工配置；
写 0 可关闭兜底。
