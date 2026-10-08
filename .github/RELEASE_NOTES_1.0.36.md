# AI Gateway 1.0.36

**主题：把「重启 / 保活 / 成本调度」这三件会打断或花错钱的事，从「到点就干」改成「确认安全且划算才干」。**

1.0.35 把网关日志补齐了时间戳、把流收尾凭据与 `/status.in_flight` 透了出来 ——
那是**观测**。1.0.36 用上了这些观测。

---

## 一、重启前排空在途请求（修「回答写到一半断了」）

### 现象与根因

网关是宿主（GUI / server）的**子进程**。宿主调 `stop_gateway()` 时直接终止它，
正在服务的 SSE 流被拦腰截断 —— 客户端看到「回答写到一半断了 / 本轮运行失败」。

A 族断流（528 条）的根因排查定位到：**每日维护窗口（本地 00:00:45）**
那两条 A 就是这个形状。

### 修法

`restart_gateway_serialized()` 在 `stop_gateway()` **之前**先读
`/status.in_flight`，非 0 就每 250ms 轮询、最多等 30 秒；归零即停。

- **不等待的出口**：旧版网关（≤1.0.34）没有 `in_flight` 字段 ⇒ 按「不知道」处理、
  不等待并留日志。**绝不把「不知道」当成 0**。
- **超时出口**：30 秒仍非 0 就照常重启并留日志 —— 无限等会让「账号变更生效」卡死。

### 实测（真链路，跑的是发行包二进制）

```text
[1] start_gateway ... 就绪: port=7863 is_running=true
[2] 流已发起 2s，/status.in_flight = Some(1)
[3] restart_gateway_serialized ...
    restart elapsed_ms=4218 ok=true
[4] 流：用时 4254 ms  saw_done=true  note=clean EOF after [DONE]
[gateway] 重启前排空：当前 1 个请求在途，等待其结束（最多 30s）
[gateway] 重启前排空完成：在途请求已归零（等待 2352ms）
```

**关键读数**：客户端拿到完整 `[DONE]`，而重启耗时 4218ms ≈ 那条流的剩余时长。

---

## 二、保活兜底：整点漏了就补，不再静默丢一天

保活只在配置整点（默认 **22:00**）触发，而网关是独立子进程 —— 那一刻没在跑
（应用关了 / 刚重启 / 维护窗口），当天保活**静默消失**，下次机会 24h 后。
而保活要防的正是「服务端清理闲置 refresh 会话」（闲置数天 ⇒ 12153 invalid_grant）。

### 修法：两个判据，各管一件事

| 判据 | 管什么 | 默认 |
|---|---|---|
| `schedule.keepalive_hours` | **节奏**：正常运行时的每日整点 | `[22]` |
| `schedule.keepalive_max_age_hours`（新） | **兜底**：距上次**成功**刷新超过它就在启动时补跑 | `24` |

网关把每次保活**成功**的时刻持久化到 `state.json` 的 `keepalive_at`；
启动时筛出超期账号补跑，**未超期一个请求都不发**。补跑在后台 spawn（首次升级
所有账号都是 0，27 个同步跑会让端口 30s 不监听）。

---

## 三、成本调度：补上三条绕过「免费优先」的缝

### 业主现场

截图显示：12 个国际版号（`deepseek-v4.1-flash` 倍率 `x0.00`，免费）与 15 个国服号
（`x0.11`，计费）共存，而**国服号有 710 条对话记录**（45–50 条/号，极均匀）。

### 定位过程（关键读数）

| 事实 | 证据 |
|---|---|
| 710 条是**真对话**，不是养号任务 | 每条平均 **27 万** input token，与真实对话同量级；养号任务走 billing 域，**不记 token** |
| 15 个号被**均匀**摊分（45–50） | 均匀 ⇒ 会话各自绑到不同号，不是偶发 |
| 不是兜底路径 | `fallback_earliest_expiry` 全量日志 **0 行**（该日志 1.0.23 起就有） |
| 1.0.36 之后 679 次 chat 全走国际版 | 19:23 起日志里 `chat uid=` 无一条国服号 |

⇒ 唯一能解释「会话级均匀分摊 + 绕过免费优先」的是**粘性绑定路径**：

```text
pick_by_uid_for_model_region 只查 healthy / in_flight / model_available
**完全不看 cost** ⇒ 会话一旦绑到计费号，30 分钟 TTL 内所有请求都走它
```

### 修法（三处）

1. **粘性快路径加成本守卫**（`pool.rs`）：绑定号是「确定计费」而池里存在
   「确定免费」的可服务候选时，返回 `None` ⇒ 调用方解绑、走完整选号。
2. **兜底路径加免费优先**（`pick_earliest_expiry`）：先跑 `free_tier` 再挑冷却最早结束的。
   依据：另一条兜底 `pick_best_from` **本来就有**，只有这条漏了。
3. **区域清单落盘复用**（`models_cache.rs` 新增 `models_cache.json`）：
   倍率是成本调度的唯一输入，重启后缓存为空会让「免费优先」静默失效；
   拉取失败 / 退避窗口内也退回**已知真值**（倍率是静态事实，不该被一次拉取失败牵连）。

### 实测（真链路，发行包二进制）

**粘性守卫**（同一会话 + 制造冷却让绑定落到计费号，再放开）：

```text
23:12:30  free 健康      → 6 次全走 free（粘性绑定到 free）
23:12:45  free 冷却      → 3 次走 paid（此时 free 不可用，paid 是唯一候选）
23:13:04  free 仍冷却    → 4 次走 paid
23:13:26  free 恢复      → 4 次全部回到 free   ← 粘性成本守卫生效
```

**清单持久化**（关掉上游再重启）：

```text
上游 DOWN 时 /v1/models/regions：
  cn:   count=1  mult=0.11   err=<拉取失败>   ← 倍率仍在（来自落盘缓存）
  intl: count=1  mult=0.0    err=<拉取失败>
```

---

## 四、依赖告警处置（3 条）

| # | 包 | 生态 | 严重度 | 处置 |
|---|---|---|---|---|
| 1 | `source-map-js` | npm（dev-only） | high | **已修**：`1.2.1 → 1.2.2` |
| 2 | `rustls` | rust | medium | **已修**：`0.23.43 → 0.23.45` |
| 3 | `glib` | rust（**仅 Linux 目标**） | medium | **不适用**（见下） |

### `glib` 为什么修不了

告警要求 **≥0.20.0**，而 `glib 0.18.5` 来自 `gtk 0.18.2` ← `tauri 2.11.5`（经
`tray-icon` / `libappindicator`）：

- `cargo update -p glib --precise 0.20.12` 直接报 `glib = "^0.18"` 约束冲突；
- 上游 `gtk` 版本线止于 **0.18.x**（crates.io 无 `gtk 0.20`）；
- `tray-icon` 0.24.2 → 0.26.1 **每一版都仍写 `gtk ^0.18`**；
- 该依赖**只在 Linux 目标**（`cargo tree -i glib --target x86_64-pc-windows-msvc` 报
  `nothing to print`）—— 本产品在 Windows 发布，暴露面为零。

---

## 五、测试基线

**workspace 3070 passed / 0 failed / 7 ignored**（15 个 suite）。

本轮新增 17 个回归测试：

- 排空重启 2 个（`restart_drains_in_flight_before_stopping` 等）
- 保活兜底 3 个（`keepalive_staleness_boundaries` 等）
- 兜底成本 4 个（`fallback_earliest_expiry_*`）
- 粘性成本 4 个（`sticky_binding_*`）
- 清单持久化 4 个（`persists_and_restores_*` 等）

---

## 六、配置新增项

```json
{
  "schedule": {
    "keepalive_hours": [22],
    "keepalive_max_age_hours": 24
  }
}
```

`keepalive_max_age_hours` 由宿主自动写入（默认 24），无需手工配置；写 0 可关闭兜底。
