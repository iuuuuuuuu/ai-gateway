# Agent Note: 设备 token 的 SDK 目录发现：扫全部客户端 + 自带垫片

Status: implemented

## Problem

原 `docs/DECISIONS.md` D19（从 DECISIONS.md 迁移）。

业主现场（第二轮，装上 D18 的包之后依旧）：

> 「我装了最新版还是这样子，还是你自己跑全流程吧」
> 附 503 `unusual_activity`，倒计时 86 秒。

先确认修复真的生效了（判据是当日构建指纹，不是「我打包了」）：

```text
:7864 PID=4772  C:\Users\<u>\.wb-switch\gateway\bin\gateway-50b509-852a8d19734f9765.exe
  SHA256 = 1268ADE9371C0C0B6573FA68A162DE7BD52BD22645C098CC18845F85349007F9
自建 embedded/gateway.exe SHA256 = 同一个值        启动 08:06:00
```

⇒ **D18 的修复确已上线，但它不是本次 3012 的根因。** 先前的诊断不完整。

真根因：国际版账号的 `X-Device-Token` **永远拿不到**。网关日志给出 **1:1 对应**
（四次 3012 **全部**紧跟同一账号的取 token 失败）：

```text
08:06:15.784  device_token uid=<uuid>: 获取失败: 缺少组件: native/turing-sdk
08:06:17.963  chat uid=<uuid>: 上游 3012 unusual activity     ← 2.2 秒后
```

且 `attach_device_token` **只对国际版发**（`client.rs` 的 `if !a.is_intl()`）——
即吃 3012 的那个账号就是国际版账号，与业主原话「国际服路由到然后直接报错」吻合。

而 `device_token_server.rs` 的文件头自己写着这条代价：

> ✅ 让网关请求与官方客户端**同形态** —— **长期缺设备凭证会被风控逐步标记**。

不修的话，每一次国际版请求都在给上游交一份「没有设备凭证」的画像，
账号会被风控逐步标记；而标记一旦形成，补 token 也救不回来（见下「仍然没解决的部分」）。

## Decision

### 两层根因（都实测确认）

**① 只认一个客户端目录。** `resolve_sdk_dir()` 原实现只找
`%LOCALAPPDATA%\Programs\WorkBuddyAI\...\native\turing-sdk`，而业主机器上
真正可用的是 **`WorkBuddyAIGloable`**（国际版）。客户端目录名与账号区域
**并非一一对应**，机器上可能装任意一个或两个都装。

**② 判据多要一个 `index.cjs`。** 实测两个目录的真实构成：

| 目录 | `index.cjs` | `build/Release/turing_sdk.node` | `TuringShieldSDK.dll` |
|---|---|---|---|
| `WorkBuddyAI`（国服） | ❌ **没有** | ✅ 158 KB | ✅ 1.44 MB |
| `WorkBuddyAIGloable`（国际版） | ✅ 1.2 KB | ✅ 158 KB | ✅ 1.44 MB |

读 `index.cjs` 全文：**它只是 1.2 KB 的 require 垫片，不含任何 SDK 逻辑**：

```js
binding = require(path.join(__dirname, 'build', 'Release', 'turing_sdk.node'));
```

⇒ 国服目录**原生文件齐全且可用**，仅因缺那个垫片就被判「缺少组件」。

### 修法（业主选「两条都改」）

1. **扫全部已知客户端**（`CLIENT_DIR_NAMES`：WorkBuddyAI / WorkBuddyAIGloable /
   CodeBuddy CN / CodeBuddy / WorkBuddy），任一个可用即采用；
2. **判据放宽为「原生模块存在即可」**（`sdk_dir_is_usable` 只看
   `build/Release/turing_sdk.node`）；
3. **自带垫片** `assets/wb-device-token/turing-sdk-shim.cjs`，worker
   **优先**用它、客户端 `index.cjs` 只作后备。

★ 垫片**不实现任何协议逻辑**，只把官方原生模块 require 出来原样转发；
参数仍逐字用官方 `cli/product.json` 的值。它不改变「原生文件仍须来自已安装
的客户端」这个事实 —— `turing_sdk.node` / `TuringShieldSDK.dll` 是
**腾讯的闭源二进制**，我们无法自行生成。

### ⚠ 第一版垫片写错了，是实测抓出来的

垫片住在**我们自己的** `assets/` 里，而原生文件在客户端目录 ⇒
第一版写 `path.join(__dirname, ...)` **必然找不到原生模块**：

```text
{"ok":false,"error":"turing-sdk 不可用: Cannot find module ...\assets\wb-device-token\build\Release\turing_sdk.node"}
```

改为由 worker 在 require 之前设置 `WB_TURING_NATIVE_DIR`、垫片据此解析。
**有门禁钉住这个顺序**（`worker_sets_native_dir_before_requiring_shim`）——
顺序写反会静默退化成「找不到原生模块」。

## Alternatives considered

- **只把 `resolve_sdk_dir()` 的搜索路径补上 `WorkBuddyAIGloable`** — 它最强的理由
  是最小改动：根因是「只找了一个目录」，那就多找一个目录，其余逻辑一字不动，
  回归面最小。否掉的原因是它把「目录名」当成「区域」来用，而两者**并非一一对应** ——
  机器上可能装任意一个或两个都装，还可能是 `CodeBuddy` / `CodeBuddy CN`。
  按目录名逐个补，下次换客户端版本又是一次同样的故障。

- **判据保持「必须有 `index.cjs`」，改为让国服目录也带一个** — 它最强的理由是
  不动判据就等于不动既有假设：`index.cjs` 一直被认为是 SDK 的入口，
  保持它意味着「入口存在才可用」这条语义不被稀释。否掉的原因有实测依据：
  读全文后 `index.cjs` 只是 1.2 KB 的 require 垫片、**不含任何 SDK 逻辑**，
  而国服目录原生文件齐全（158 KB 的 `turing_sdk.node` + 1.44 MB 的 DLL）。
  以垫片是否存在判可用性，等于用一个不含逻辑的文件当判据 —— 判据本身是错的。

- **要求用户自己装官方客户端、并复制一份 `index.cjs` 到国服目录** — 它最强的理由是
  零代码改动：既然官方客户端里有那个文件，把它放到位就行，也不必让网关去理解
  目录布局。否掉的原因是它把修复责任推给用户，且依赖一个「用户能找到并正确复制
  1.2 KB 垫片」的假设；而同一件事我们用一个自带垫片就能做，且不会因为用户复制错
  位置而失败。

- **让垫片内联实现一份 turing-sdk 的协议逻辑** — 它最强的理由是彻底摆脱对已安装
  客户端的依赖：不必扫目录、不必管 `WB_TURING_NATIVE_DIR`、也不存在「原生文件缺失」
  这类故障，部署形态最干净。否掉的原因是**做不到**：`turing_sdk.node` /
  `TuringShieldSDK.dll` 是**腾讯的闭源二进制**，我们无法自行生成；垫片只能做转发，
  不能替代实现。

- **垫片里用 `path.join(__dirname, ...)` 解析原生模块** — 它最强的理由是最自然：
  同目录相对定位是 Node 里最常规的写法，且如果垫片和原生模块一起分发，它天然成立。
  否掉的原因是部署事实与之相反：垫片住在**我们自己的** `assets/`，原生文件在
  **客户端目录**，`__dirname` 指向 `assets/wb-device-token/` ⇒ **必然**找不到，
  且报错是「Cannot find module」这种看起来像打包漏文件的形式。

## Consequences

- **收益**：国服目录（原生文件齐全、只缺垫片）从「缺少组件」变成可用 ——
  这正是业主机器上的实际处境，即修复目标本身。
- **收益**：目录发现不再假设「客户端目录名 = 账号区域」，装任意一个或两个都装
  都能找到可用 SDK；五个已知目录名任一可用即采用。
- **收益**：判据从「垫片存在」回到「原生模块存在」，与真正决定可用性的东西对齐。
- **代价与已知上限**：垫片与 worker 之间多了一条**隐式契约** ——
  worker 必须在 require 垫片**之前**设置 `WB_TURING_NATIVE_DIR`。顺序写反不会报错，
  只会静默退化成「找不到原生模块」，因此有门禁
  `worker_sets_native_dir_before_requiring_shim` 钉住顺序。
- **代价与已知上限**：修复的作用是**阻止新的标记产生**，不是治疗。
  **已被上游标记的账号，补上 device token 也救不回来** —— `device_token_server.rs`
  文件头写明（2026-09-28 实测，9 个国际版账号三组对照）：

  > ❌ **不能**救活已被上游标记失效的账号：那是服务端的 (设备,账号) 绑定状态。
  > 403 11140 的账号带上任何 device token 仍然 403；只有重新登录才能恢复。

  若业主当前账号已被标记，需要**重新登录**该账号才能恢复 ——
  不能让业主以为装上就好。
- **信号（翻案条件）**：若实测发现补上 token 后**仍然**持续 3012，则说明标记已存在
  （走上一条），或 turing-sdk 参数（channelId / productName / productVersion）
  需要跟随客户端版本更新 —— 届时以抓包对照为准，**不要凭推断改参数**。

## Verification

**真跑 worker，四个场景 + 两次故障注入**：

| 场景 | 结果 |
|---|---|
| 国服目录 + 自带垫片（**修复目标**） | ✅ `{"ok":true,"tokens":["v3:..."]}` |
| 国际版目录 + 自带垫片 | ✅ 成功 |
| 国际版目录 + 客户端 `index.cjs`（回归） | ✅ 成功（老路径未破坏） |
| 国服目录 + 无垫片（= 修复前） | ❌ 失败 —— **即修复前业主的处境** |

故障注入：

- 判据退回「必须有 index.cjs」⇒ 2 条门禁变红 ✅
- 目录清单退回「只认国服」⇒ `client_dir_names_cover_cn_and_intl` 变红 ✅

顺序门禁：`worker_sets_native_dir_before_requiring_shim`。
