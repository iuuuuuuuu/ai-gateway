# Agent Note: 账号菜单传了本地主键而不是上游 UID——每个账号都报「不在账号池内」

Status: implemented

## Problem

业主现场（2026-10-19，紧接上一个修复之后）：

> 「我执行国内账号，这几个活动，全都说不在账号池内」

截图里账号卡片菜单的六个入口全部失败：活跃上报 / 夜猫子任务 / 开学季活动 /
trial 加油包 / 活跃地图 / 校园日活动。

报错来自 `crates/ai-gateway-router/src/scheduler/mod.rs`：

```text
该账号不在网关账号池中（可能已禁用、需重新登录或尚未同步）
```

### 根因：`id` 与 `uid` 是两个不同的主键

- **网关** `/tasks/run` 的 `accountId` 语义是**上游 UID** —— 判据是
  `self.auth_by_uid(account_uid)`，而 `Pool::auth_by_uid` 查的是 `by_uid` 表。
- **前端** `account-card.tsx` 的八个入口传的是 `account.id` —— 宿主**账号库的主键**。

本机实测：**27 个账号的 `id` 与 `uid` 无一相同**（都是 UUID，肉眼完全无法分辨）：

```text
wish:  id=da167ea0-c331-4994-813e-7691b13a3951
       uid=3b5d6048-7813-4e5b-848e-c21bdd5fc7c8
```

⇒ `auth_by_uid` 永远查不到 ⇒ 每个账号都报「不在账号池中」。

⚠ 报错文案里列的三个原因（已禁用 / 需重新登录 / 尚未同步）**一个都不成立** ——
账号完全正常。这正是它误导用户的原因：用户会去检查登录状态，而那毫无问题。

### 决定性对照实验

同一账号、同一任务，只改传的值：

| 传入 | 网关返回 |
|---|---|
| `accountId = id`（修复前前端行为） | `{"ran":false,"skip":"account_not_found"}` |
| `accountId = uid`（网关池里的值） | `{"ran":true}``"message":"已触发该账号的一轮"` |

## Decision

前端八个入口统一改传 `uid`，并把「该传哪个标识」**收敛成一个函数**
`gatewayAccountId(account)`，不再散落在各调用点。

```ts
function gatewayAccountId(account: AccountMeta): string {
  return (account.uid ?? "").trim() || account.id;
}
```

改动清单（`src/components/account-card.tsx`）：

| 位置 | 改动 |
|---|---|
| 5 个调度器任务菜单（activity / nightowl / school / trial / growthmap） | `account.id` → `gatewayAccountId(account)` |
| 2 个成长任务入口（school_season / 逐项 `t.task_code`） | 同上 |
| 「一键执行全部任务」入口 | 同上 |
| 4 处 `growthAllRunningAccountId === account.id` 运行态比较 | 同上（否则转圈状态会失效） |

### 为什么收敛成函数而不是就地改八处

`id` 与 `uid` 都是 UUID，**长得一模一样**。散着写等于把「这两个不能混」的知识
钉在八个位置 —— 只要有一处漏掉（或者将来新增菜单项时照抄旁边的 `account.id`），
同样的 bug 就会以另一个入口复现。

项目里其实**早就记录了这个区别**：`accountDetailRows` 里有一行
`["账号 ID", displayText(account.id), "本地账号库的主键（与上游 UID 不同）"]`。
知识是有的，缺的是**把它变成一个不可绕过的构造**。

## Alternatives considered

- **在网关侧做 id → uid 的映射** — 它最强的理由是**兼容性最好**：前端全部调用点
  不用改，将来任何误传 `id` 的地方都会自动被纠正，错误不可能再溜到用户面前。
  否掉的原因是网关**只认 uid 是它的既有契约**（`auth_by_uid` 的入参、`Auth` 的
  字段名、凭证文件名 `workbuddy-{uid}.json` 都是 uid），而网关侧**没有账号库**，
  拿不到 `id → uid` 的映射表——那份映射只在宿主手里。⇒ 要在网关侧做，就得把
  账号库复制一份进网关，凭空引入第二份状态，且它与宿主那份必然漂移。

- **在宿主侧（`/api/gateway/task-run`）做转换** — 它最强的理由是**收口位置更靠后**：
  宿主是唯一同时知道 `id` 和 `uid` 的地方（`accounts.json` 两个字段都有），
  在这里转换一次，前端八处都不用改，也不需要知道这个区别。
  否掉的原因是**宿主 HTTP 服务在打包版里不一定在跑**（实测本机 7863 未监听，
  用户跑的是桌面壳，7863 由壳内部代理）。更关键的是：`api_gateway_task_run`
  的参数在代码里就叫 `account_uid`（`run_task_for(task: &str, account_uid: &str)`），
  文档与注释也写明「单账号时把 uid 放进 body」—— **它本来就不接受 `id`**，
  在这里加转换等于给一个「本该传 A 却传了 B」的问题打静默补丁，把契约搞模糊：
  下一个调用方传 `uid` 时，转换逻辑就成了永远不触发的死代码。
  ⇒ 让调用方传对，比让被调用方猜要可靠。

- **只在报错文案里提示「请检查账号标识」** — 它最强的理由是**改动最小**：
  一行文案，不碰任何逻辑。否掉的原因是它把一个**必然会错 100% 的调用**变成
  「用户自己去排查」——而用户能做什么？两个 UUID 他分不出谁是谁，
  报错文案也无法告诉他正确值长什么样。这不是提示不足，是接口契约被违反。

## Consequences

- **收益**：账号卡片的八个养号/成长任务入口恢复可用（实测 3/3 账号由
  「id 失败」变为「uid 成功」）。
- **收益**：`gatewayAccountId` 让「id ≠ uid」成为**一个不可绕过的构造**，
  新增菜单项时照抄 `gatewayAccountId(account)` 即可，不会再混用。
- **代价**：多一个函数与一次函数调用（可忽略）。
- **代价**：前端多了一条心智负担——「有些接口要 id、有些要 uid」。
  这是**既有的真实约束**（宿主侧 `setAccountNote` / 账号记录等确实用 `id`），
  本次只是把边界显式化。**信号**：若将来 `id` 与 `uid` 能统一（宿主改用 uid 作主键），
  这个函数可以退化成恒等函数，届时应删掉它而不是留着。
- **已知上限**：本次只改了**能自我验证**的调用点。若宿主还有别的接口收
  `accountId` 但实际要 uid（例如未来的新接口），同样的混用仍可能发生 ——
  根治办法是让网关侧对未知标识给出**可区分**的报错（例如同时打印收到的值与
  池内 uid 列表的前几个），但那会泄露账号信息，未做。

## Verification

### 构建

`npm run build` → exit 0（`tsc && vite build` 通过）。

### 实机端到端（真实网关进程 :7864，真实凭证、真实 HTTP）

三个账号（`13213390865` / `wish` / `苏打`），每个跑两次「活跃上报」，
只改传入的标识：

| 账号 | 传 `id` | 传 `uid` |
|---|---|---|
| 13213390865 | ❌ `ran:false` | ✅ `ran:true` |
| wish | ❌ `ran:false` | ✅ `ran:true` |
| 苏打 | ❌ `ran:false` | ✅ `ran:true` |

**3/3 完全一致**。

### 顺带确认：调度器任务链路本身是好的

用国服账号 `wish` 逐个触发（直连网关、传 uid）：

| 任务 | 返回 | 判断 |
|---|---|---|
| `activity` | `ran:true` | ✅ |
| `nightowl` | `skip:"outside_window"`「当前不在夜猫子时段（23:00–08:00）」 | ✅ 14 点本就不在时段 |
| `school` | `ran:true` | ✅ |
| `trial` | `skip:"region_mismatch"`「trial 仅国际版可用」 | ✅ 菜单里也写了「国际版专享」 |
| `growthmap` | （无返回） | ⚠ 未验证，见下 |

⚠ `growthmap`（活跃地图）那次 curl 返回空，**未能确认**。
原因未查明（可能耗时超过 curl 超时，也可能是该任务的响应体为空）。
本条**不计入通过项**。

### 排除的干扰项

- **不是账号池为空**：网关 `/healthz` 返回 `healthy:32, total:33`，
  `/status`（带 key）返回 33 个账号（workbuddy 27 / qoder 2 / zcode 3 / trae 1）。
- **不是账号被勾选过滤**：那是上一个修复（`2026-10-19-manual-mode-accounts-still-run-tasks.md`）
  的问题，本次是**另一个**独立缺陷，两者症状相同但根因不同。已通过
  「传 uid 即成功」排除——若账号不在池里，传 uid 也会失败。
- **不是区域判定错误**：第一次复现时我随机挑到了 `domain=www.workbuddy.ai` 的
  **真国际版**账号，它报 `region_mismatch`「该任务是国服专属」是**正确行为**。
  改用国服账号后立刻复现真实缺陷——**这个假阳性一度让我差点查错方向**。