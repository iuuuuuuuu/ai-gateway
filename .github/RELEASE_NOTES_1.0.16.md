## AI Gateway 1.0.16 · 正式版

本版处理你报的 **2 项**（Trae 页面崩溃 + 账号页布局），外加一处**在排查过程中挖出来的
根因级缺陷**：SPA 深链在 webui 模式下**全部失效**（用浏览器打开 `/trae`、`/agents`、
`/settings` 会变成下载文件）。这一项影响面最大，也是前几次「页面空白」误判的真凶。

> ⚠ **如果你装的是 1.0.14 或更早**：请升级。
> 1.0.14 里**没有**「设置页留白」等 4 项修复 —— 那段源码写在 10-01 19:40:10，
> 而 1.0.14 构建于 **19:14:10**，晚了 26 分钟。1.0.15 补齐了那 4 项，
> 1.0.16 又修掉了 Trae 页面崩溃与布局。从 1.0.14 直升 1.0.16 收益最大。

---

## 一、你报的 2 项

### ① 点 Trae 积分后页面报错 —— 已修

**你的原话**：

> 页面出错了 / 这一页在渲染时抛了异常，已被拦截 —— 应用其余部分仍可使用。
> …错误摘要 `TypeError: Cannot read properties of null (reading 'length')`
> … **可以登陆了，点击trae积分后报错**

**根因不是前端写错了，是后端契约违约。** `trae_credits.rs` 在拿不到权益包时
用 `.as_ref().map(...)` 包了一层，于是下发的 `packs` 是 `null`；前端直接读
`detail.packs.length` ⇒ 对 `null` 取 `.length` ⇒ 抛异常。

一共查出 **4 处形状不匹配**，全部修掉：

| # | 位置 | 违约 | 修法 |
|---|---|---|---|
| 1 | `trae_credits.rs` | `packs` 可能为 `null` | 抽出 `credit_detail_json()`，**恒为数组** |
| 2 | `trae_credits.rs` | `errors` 可能为 `null` | 同上，拿不到就是 `[]` |
| 3 | `api.ts:3386-3387` | 前端无兜底 | `Array.isArray(raw?.packs) ? raw.packs : []` |
| 4 | `api.ts:1433` | `TraeAccountMeta` 缺后端已下发的 9 个字段 | 补齐 + 删掉重复的 `TraeAccountMetaFull` |

现在的契约是硬保证：`packs` / `errors` **恒为数组，永不为 `null`**，并新增
**5 条回归测试**把它钉住（`13 passed / 0 failed`）。

**实测**：

```
⑤ 积分区渲染（四态之一）
   卡片[0] 状态 = 正常（有积分包），积分包文案 = "0 个积分包"
   ✅ 通过

 页面运行时异常: 无
```

最后一行是关键：你报的那条 `Cannot read properties of null (reading 'length')`
**已不再复现**。

> 截图里那些 `404 Not Found … TLB` 是**上游 Trae 的真实回应**，不是本程序的错误 ——
> 「付费身份」「权益包」两个接口在你的账号上确实不存在。这属于「拿不到数据」，
> 现在会优雅降级成 `[]` + 一行提示，而不再崩页面。

### ② Trae 账号页改成 WorkBuddy 那种布局 —— 已改

**你的原话**：

> 然后这个trae账号界面优化一下，也改成workbuddy那种一样的布局，现在的布局不方便

之前每个账号是一条**横排长条**，容器 `mx-auto max-w-5xl p-6`，宽屏上左右各留 >300px 空白；
头像还**硬编码**了 `variant="work"`（Trae CN 账号也显示 Work 图标，顺手修掉了）。

| 部位 | 之前 | 现在 |
|---|---|---|
| 布局 | 横排行列表 `space-y-2` | **卡片网格** `grid auto-rows-fr gap-5 grid-cols-[repeat(auto-fit,minmax(min(100%,340px),1fr))]` |
| 容器 | `mx-auto max-w-5xl p-6` | `w-full space-y-6 px-5 py-6 sm:px-8 sm:py-8` |
| 页头 | `text-xl`（20px） | `text-[28px]` |
| 头像 | `TraeMark variant="work"`（硬编码） | `TraeMark variant={activeApp.variant}`（按当前 app 走） |
| 积分 | 点气泡弹 Popover | 卡片内**内联展开**（`flex-1`，撑满详情区） |
| 次要操作 | 全部平铺在行内 | 收进右上 `⋯` 菜单（4 项） |
| 页脚 | 无 | `["切换","保存登录态"]` |

**实测**（CDP 驱动真实 DOM，隔离实例，视口 1636×1000）：

```
② 卡片形态
   data-slot=trae-account-card 卡片数 = 2
   旧横排行残留数 = 0（期望 0）
   卡片高 = [452.5,452.5]，宽 = [658.5,658.5]
   网格父级 = {"display":"grid","autoRows":"minmax(0px, 1fr)","alignItems":"normal"}

③ 同一排卡片等高（grid auto-rows-fr）
   卡片数 = 2，最大行内高度差 = 0px（期望 0）
   成行 = ["620px: [452.5,452.5]"]

④ 页面留白与页头
   视口宽 = 1636px，<main> 可用宽 = 1401px（矩形 1416px，滚动条占 15px）
   页面容器宽 = 1401px，铺满 main = true
   容器 max-width = none，margin-left = 0px，padding-left = 32px
   距视口左/右 = 220px / 15px（旧 max-w-5xl 居中，两侧各 >300px）
   h1 文案 = "Trae 账号"，字号 = 28px（旧 text-xl = 20px）

⑥ ⋯ 更多操作菜单   菜单项数 = 4
⑦ 卡片页脚         按钮 = ["切换","保存登录态"]
⑧ 区域徽标         "国内版（推断）"

 页面运行时异常: 无
 通过 13 项 / 不通过 0 项
```

> ★ 为什么用「2 张卡片」测：隔离实例原本只有 1 个账号，
> **单卡片的「同排等高」是空洞结论**（一张卡和谁比？）。
> 为此往隔离实例里加了一个合成账号（名字「布局实测号（合成，不可用）」，
> **只存在于隔离实例的 home 目录，不碰你的任何数据**），这才真正测出「同排高度差 = 0px」。

---

## 二、排查中挖出的根因级缺陷：SPA 深链全部失效 —— 已修

**症状**：用浏览器打开 `/trae`、`/agents`、`/settings` 等任意深层路由，
页面**被下载**而不是渲染 —— CDP 里报 `net::ERR_ABORTED` + `isDownload: true`。

**根因**：`ai-gateway-server\src\api.rs` 的 `static_handler` 按**请求的路径**决定
`Content-Type`。深链 `/trae` 在磁盘上没有对应文件，会回退到 `index.html`，
但响应头已经被写成 `application/octet-stream` ⇒ 浏览器把它当文件下载。

**修法**：新增 `fn resolve_served_path(requested, has_asset)`，先算出**实际服务的**
路径，再由它决定 `Content-Type`。新增 3 条测试（`28 passed / 0 failed`）。

> ★ 这个缺陷**误导过我自己的排查**：早先几次探针报「页面空白 / 选不中目标」，
> 我一度怀疑是 WebView2 调试端口冲突、缓存、渲染进程崩溃、演示模式……
> **全部是错的假设**。真正原因就是这一行 Content-Type。修掉之后探针一次全绿。

---

## 三、豆包：两边都不做（附证据）

Trae 两项都已安排（见文末），**豆包经独立复核判定不可行** —— 不是没找到，是确实没有：

| 检查项 | 结果 |
|---|---|
| 注册表 `HKCU:\Software\Classes\doubao\shell\open\command` | `"D:\APP\Doubao\app\Doubao.exe" -- --open-url="%1"` |
| `D:\APP\Doubao` 目录 | **不存在**（卸载残留） |
| `%LOCALAPPDATA%\Doubao` / `%APPDATA%\Doubao` / `%LOCALAPPDATA%\Programs\Doubao` | 全不存在 |
| 进程 | 无 |
| 四处常见安装根目录深度 2 内搜 `Doubao*.exe` | **0 个** |
| 仓库里已有的豆包端点 | 3 个，**没有一个是补全 API** |

⇒ 本机没有可用的豆包客户端可供对接；仓库里也没有可用的豆包对话协议。
你已确认「两边都不做」，故本版**不含任何豆包改动**。

---

## 四、实测清单（全部真机，非单测）

| # | 项目 | 结果 |
|---|---|---|
| 1 | `cargo test -p ai-gateway-core --offline trae_credits` | **13 passed / 0 failed** |
| 2 | `cargo test -p ai-gateway-server --offline` | **28 passed / 0 failed** |
| 3 | `npx tsc --noEmit` | exit 0 |
| 4 | `npx vite build` | exit 0 |
| 5 | CDP UI 探针（真实实例，2 张卡片） | **13 项通过 / 0 项不通过**，运行时异常 = **无** |
| 6 | 安装包独立解包核验（7-Zip） | 15 files，内嵌资产名命中 |
| 7 | 你的服务仍在跑 | `:3080 = 200`、`:7864 = 200` |

**隔离环境**（全程没碰你的活动数据）：

```
exe   test-bin\inst-trae-card\aigw-trae-card.exe
      SHA256 F8F305D52B3EFCA0DFE00A4C8A4FB2337E1C1FF6CD06C6662198126FBA19BA08
PID   31964      home  test-bin\inst-trae-card\home      端口  57890
```

★ 这是**编译出来的服务端**跑 webui 模式，不是第二个 Tauri 实例 ——
Tauri 实例受互斥锁 `com.momo0410.aigateway-sim` 限制，同机只能起一个。
另外 `API_BASE` 在 `api.ts:66` 是**硬编码** `http://127.0.0.1:57890`，
所以隔离实例必须占用 57890（你的 `:7864` 与 `:3080` 全程在线，未受影响）。

---

## 五、仍未验证面（如实列出）

1. **Trae 进「智能体管理」尚未完成** —— `ak` 字段的 AES-256-GCM 加密方案
   **已完全破解并 3/3 验证**，但「写进去 → Trae 里对话成功」这条端到端路径**还没跑过**。
   唯一不确定的是 `custom_model_id`（由 Trae 服务端下发，本地写入时是否被校验未证）。
2. **Trae 进「兼容网关」尚未完成**（`products\trae\` 适配器）。
3. `sk` 字段用同一套加解密，但活动记录都是 `auth_type:0` / `sk:""` ⇒ 未实证。
4. DST 跨月步进、`git filter-repo` 清理 —— 仍未验证。
5. 前端**没有测试框架**，UI 验证靠一次性 CDP 脚本。

---

## 六、安装

直接运行 `AI Gateway_1.0.16_x64-setup.exe` 覆盖安装即可。

---

## 七、接下来

按你「**两个都做，先智能体管理、后兼容网关**」的决定：

- **A · Trae 进「智能体管理」**（进行中）—— 点接入后把网关写进 Trae 的
  `state.vscdb` 自定义模型表，使 Trae 能通过 `http://127.0.0.1:7864/v1` 对话。
- **B · Trae 进「兼容网关」**（进行中）—— 新写 `products\trae\` 适配器，
  含上游 SSE → OpenAI SSE 翻译、区域主机表、4 个协议陷阱的处理。
- 豆包：两边都不做。

两项完成后会 `tsc` + **真实实例实测** + 重新打包 + 交付说明，再给你一版。
