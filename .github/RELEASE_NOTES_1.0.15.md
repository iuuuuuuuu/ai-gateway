## AI Gateway 1.0.15 · 正式版

本版处理你报的 **5 项**里的 4 项（第 5 项是提问，见文末），外加**独立的 Trae 登录链路修复**
（你先后报了 4 个症状，是同一条链路上的 4 个不同缺陷）。

> ⚠ **如果你装的是 1.0.14**：请升级。1.0.14 里**没有**「设置页留白」这项修复 ——
> 那段源码写在 10-01 19:40:10，而 1.0.14 构建于 **19:14:10**，晚了 26 分钟。

---

## 一、你报的 5 项

### ① Token 用量：勾选会串行 —— 已修

**你的原话**：「token用量这里一个模型拆分出来两个统计,而且勾选deepseek-v4.1-falsh
就会把国内国外同时勾选,这是错误的」

「拆成多个统计」**不是 bug**，是既有设计：网关的 `modelBilling` 按
`(平台, 区域, 倍率)` 三维记账，前端把同一模型的多份归属展平成多行，
**目的就是让你看到不同倍率**。你的真实数据里 `deepseek-v4.1-flash` 有 **5 份归属**。

**真正的 bug 是勾选**：筛选状态按 `model.key` 存 ⇒ 点一行，该模型**所有行**同时亮。
已改为按 `rowKey`（`模型名\0平台\0区域\0倍率`）区分。

**实测**（CDP 驱动真实 DOM，隔离实例种入你的真实 `usage.json`）：

```
[步骤1] 点第 0 行 → 全表选中 [0]；该模型 2 行里只选中 1 行   ✅
[步骤2] 点第 1 行 → 全表选中 [1]（改选，而不是取消）        ✅
[步骤3] 再点第 1 行 → 全表选中 0 行（toggle 取消）          ✅
```

### ② 智能体网关：卡片高度不一致 —— 已修

**实测**：带 `h-full` 卡片 **234 个**、**76 行**、**每行高度差全 0**。

★ 反直觉的实测结论：这个网格其实**不靠 `h-full` 等高**（带与不带都是 0.00px）。
真正让它等高的是**没有 `items-start`**（保留默认 `stretch`）。补上 `items-start` 后：
无 `h-full` ⇒ 行内高度差爆到 **416px**；有 `h-full` ⇒ 回到 0.00px。
故 `h-full` 是**冗余但有用的保险**，两处都保留。

### ③ Trae / 豆包 放到 ZCode 账号下面 —— 已修

导航 DOM 次序实测：`/zcode` → `/trae` → `/doubao` ✅

### ④ 设置页两侧留白过多 —— 已修（本版首次到达你）

**实测**：视口 2324px、内容容器 **2040px**、**3 列**、左右留白 252px/32px。
（旧版 `max-w-3xl` = 上限 768px。）

改法：外层去掉宽度上限 + 内容区改 **CSS 多列瀑布流**
（`columns-1 / sm:columns-2 / xl:columns-3`），每张卡片包 `break-inside-avoid`
—— 否则一张卡片会被**从中间劈开**跨到下一列。

### ⑤ 账号池：平台 / 国内国外 区分不明显 —— 已修

| | 之前 | 现在 |
|---|---|---|
| 平台 | `WB` / `QD` / `ZC` 缩写，9.5px | **全名** `WorkBuddy` / `Qoder` / `ZCode`，10.5px，各自配色 |
| 区域 | **完全没有** | 独立徽标 `国服` / `国际版`，10.5px |

**实测**：

```
账号池行数 = 31；区域徽标数 = 31/31 行；平台徽标数 = 31
区域文案分布 = {"国服":17,"国际版":14}    ← 与后端 /status 的 cn=17/intl=14 逐数吻合
平台文案分布 = {"Qoder":2,"WorkBuddy":27,"ZCode":2}
```

★ 后端也改了：`crates/ai-gateway-router/src/server.rs:366-367` 新增**恒输出**
`product` + `region`。老网关不下发 region ⇒ 前端 `undefined` 时**直接不渲染区域徽标**，
刻意**不猜**（错标比不标更误导）。

---

## 二、Trae 登录链路（本轮最大的一块）

你先后报的 4 个症状，是**同一条链路上的 4 个不同缺陷**：

| 你报的 | 根因 | 修法 |
|---|---|---|
| 「刷新就回到 trae 官网首页」 | 授权 URL 用了**错的产品线参数**（`auth_from=trae` + `client_id=ono9krqynydwx5`），而你装的是 **TRAE SOLO CN**，需要 `solo` + `en1oxy7wnw8j9n`。且整个 URL 形状都不对（`/authorize`→`/authorization`、`redirect_uri`→`auth_callback_url`、`state`→`login_trace_id`，还缺 `login_version/auth_type/redirect/PKCE/device-fingerprint/plugin_version`） | 新增 `TraeFlavor` 枚举 + `detect_flavor()` 自动判产品线 |
| 「访问超时了」 | 两条独立原因叠加：① `api.trae.cn` 的 **IPv6 完全不通**（握手 15s 超时），reqwest 默认 IPv6 优先且**没有 Happy Eyeballs**；② `http_request_direct` 号称直连，**实际仍在走系统代理** | 同时加 `.local_address(Ipv4)` + `.no_proxy()` |
| 「找不到授权码（code）」 | 解析只找嵌套路径 `["authCodeInfo","AuthCode"]`，而真实形状是 **query 参数** `authCodeInfo`，其解码后的 JSON 里 `AuthCode` 在**顶层** | 加顶层键扫描（去 `_` 转小写比对 `authcode`） |
| 「`10101 无效参数：{__Message.field}.`」 | 我们发的是 **OAuth 2.0 标准信封**，而这个端点收的是**私有信封** | 按真实客户端 `main.js` 重写信封 |

### 2.1 信封正确性怎么证明的（关键）

真实客户端 `main.js` 的 `exchangeTokenByAuthCode`：路径 `/trae/api/v3/oauth/ExchangeToken`，
body `{ClientID, AuthCode, CodeVerifier, DeviceInfo, IDEVersion}`，
headers 只有 `Content-Type` + `x-cloudide-token`（我们此前臆造的 `X-App-Id` 在 main.js 里 **0 处**）。

**验证方法**：授权码是 600 秒一次性窗口，没法进 CI；但**两条路径共用同一套字段名/路径/DeviceInfo**，
故用 **refresh 路径 + 垃圾 token** 探测 —— 信封若被正确解析，服务端会走到「校验 RefreshToken」
并回**针对 token 的错误**；若信封还不对，就仍是笼统的 `10101`。

```
[① 基线] 不存在的路径              HTTP 404 len=18
[② 新信封 + 假 RefreshToken]
  https://api.trae.cn        HTTP 401  code=20101  msg=Token 无效：refresh token is invalid.
  https://api.trae.com.cn    HTTP 401  code=20101  msg=Token 无效：refresh token is invalid.
[③ 对照：旧 OAuth 标准信封]  HTTP 400  code=10101  msg=无效参数：{__Message.field}.
[④ 对照：空 body]            HTTP 400  code=10101  msg=无效参数：{__Message.field}.
```

⇒ **两个 host 都把新信封解析到底并明确判定 refresh token 无效**，旧信封与空 body 都是 `10101`。
`20101` 正是真实客户端认的 `RefreshTokenInvalid` 码之一。**信封完全正确，已被上游接受。**

### 2.2 另外 3 处一并修掉

1. **`fetch_user_info` 整个请求形状都是错的**：路径 `/cloudide/api/v3/trae/user/info`
   （**不存在**）→ `/cloudide/api/v3/trae/GetUserInfo`；GET → **POST**；
   `Authorization: Bearer` → **`x-cloudide-token`**；响应层 `data.result` → **顶层 `Result`**；
   uid 键 `id` → **`UserID`**；昵称键 `name` → **`ScreenName`**。
2. **回调 `userInfo` 优先**：真实客户端是 `r ?? await getUserInfo(...)` ——
   **回调带了 `userInfo` 就根本不调 GetUserInfo**。已接入回调解析（`user_info_from_callback`），
   uid 三级兜底：JWT → 回调 → GetUserInfo。
3. ★ **安全修复**：`mask_sensitive` 此前**不认识裸 `Token`**（真实响应 `Result.Token`
   就是 access token）⇒ access token 会**明文显示在界面上**。已补上 `Token` + `ClientSecret`。

### 2.3 仍需你配合一次

授权码路径的**成功**分支还差最后一步 —— 需要一个**新鲜**授权码（600 秒一次性，我拿不到）。
你手上那两个（m30010 / m30144）都已过期，用过期码探测新老信封**都返回 `10101`**
（该端点对「空 body」与「完整错误字段」返回逐字相同的响应），**无判别力**。

⇒ 请打开 Trae 账号页点一次登录，把结果告诉我。信封本身已被上面 refresh 路径的探测
**证明正确**，这一步是为了确认端到端闭环。

---

## 三、其它一并修掉 / 加固的

### 3.1 `reqwest` 偷偷走系统代理（影响面比 Trae 大）

`http_request_direct` 的注释写着「这就是直连的定义」，但**代码里既没调 `.proxy()`
也没调 `.no_proxy()`** ⇒ reqwest 默认 `auto_sys_proxy = true`，在 Windows 上
**读注册表**（`HKCU\...\Internet Settings`）⇒ 系统代理一直生效。

**为什么一直没暴露**：`hyper-util` 全 workspace 只有一个版本，`src-tauri` 的
`tauri-plugin-updater` 拉 reqwest 0.13.4 时打开了 `client-proxy-system`，
Cargo 按包版本统一 feature ⇒ 我们自己的 reqwest 0.12.28 **在 `--workspace` 构建里
也拿到了**系统代理能力（`-p` 构建里没有）。这也是**同一份源码、两种构建行为不同**的来源。

**实测**：

```
① 直连          HTTP 400  100.101s
② 系统代理 7890  HTTP 400    0.076s
① 直连(复测)     HTTP 400  100.136s
② 系统代理(复测) HTTP 400    0.058s
```

⇒ 直连比走代理慢 **1300 倍**。

### 3.2 IPv4 / IPv6（超时的真正根因）

```
① DNS  A(IPv4) 5 条: 123.53.183.202/203/206/208/209
        AAAA(IPv6) 5 条: 240e:93d:9:10:8000:0:b00:138/143/66/67
② TCP 握手（裸 socket）
   123.53.183.209/208/203   OK    0.009s / 0.030s / 0.006s
   240e:93d:9:10:8000:0:b00:67/143  FAIL 15.001s / 15.005s  TimeoutError
③ 完整 HTTPS（POST ExchangeToken，绕开代理）
   强制 IPv4 直连        HTTP 400  0.073s
   默认(IPv6 优先)直连   HTTP 400  105.249s
```

★ **`.no_proxy()` 与 `.local_address(Ipv4)` 缺一不可**：只加 `.no_proxy()` 会掉进
「直连 + IPv6 优先」= 105s；只加 `.local_address(Ipv4)` 则仍被不通的系统代理拖死。
两处都已落地：`config.rs:1230` 与 `trae_refresh.rs:232-233`。

### 3.3 设备指纹保真（`DeviceInfo` 12 个字段）

此前 `DeviceModel` / `DeviceBrand` / `DeviceCPU` 都是**空串**，`OSInfo` 是首字母大写的 `"Windows"`。

| 字段 | 现在取的值 | 依据 |
|---|---|---|
| `DeviceModel` | 注册表 `SystemProductName`（回退 `BaseBoardProduct`） | 真实客户端取 WMI `Win32_ComputerSystem.Model`；**实测本机两者逐字相同**⇒ 不引入 WMI 依赖 |
| `DeviceBrand` | `SystemManufacturer`（回退 `BaseBoardManufacturer`） | 同上 |
| `DeviceCPU` | `ProcessorNameString`，按真实客户端 `KBe` 归一化（`@` 之后截掉） | Node `os.cpus()[0].model` 在 Windows 上就是它 |
| `OSInfo` | **`"windows"`**（小写） | `main.js`：`Id==="darwin"?"mac":Id==="win32"?"windows":Id` |

### 3.4 新增回归闸门

`换_token_请求报文与真实客户端逐字段一致` —— 用**本地 mock 上游**收一个真实请求，逐字段断言：
请求行、headers 集合（含**不含** `x-app-id`）、body 键集合**恰好** 5 个、
`DeviceInfo` 键集合**恰好 12 个**、`PlatformCode=="SOLO_PC"`、公钥是 SPKI PEM、
以及「设备字段不能是空串、不能带首尾空格」。

★ 这条测试**抓到了两处我自己的错误期望**（不是产品缺陷）：
① 我断言 `exchange_auth_code` 返回带前缀的 token —— 实际返回**裸 token**，加
`Cloud-IDE-JWT ` 前缀是调用方的事；② 我断言 `DeviceInfo` 是 11 个字段 —— 实际 **12** 个
（漏了 `PlatformCode`）。**实现是对的，测试期望是错的** —— 这正是 mock 断言的价值。

---

## 四、实测：全链验收

| 项 | 手段 | 结果 |
|---|---|---|
| ① 勾选串行 | CDP 三步断言 | ✅ 3/3 |
| ② 卡片等高 | CDP 量 234 卡片 / 76 行 | ✅ 每行差 0px |
| ③ 导航次序 | CDP 读 DOM | ✅ zcode→trae→doubao |
| ④ 设置页留白 | CDP 量容器 | ✅ 2040px / 3 列 |
| ⑤ 账号池标识 | CDP 数徽标 | ✅ 31/31，分布与后端吻合 |
| 全链路真实对话 | 6 个模型真发请求 | ✅ 4×200 / 2×402 |
| 全页面白屏扫描 | 11 个路由 | ✅ 11/11 |
| 内嵌网关指纹 | 宿主 materialize 后比对 | ✅ 逐字节相同 |
| 包内源码新鲜度 | mtime vs 构建时间 | ✅ 0 个文件晚于构建 |
| `cargo test --workspace --lib --offline` | — | ✅ **17 + 1159 + 1465 = 2641 passed / 0 failed / 4 ignored** |

### 4.1 全链路真实对话（隔离网关 `:7865`）

```
HTTP 200  1195ms  workbuddy:deepseek-v4.1-flash         resp=deepseek-v4.1-flash  content="收到"  total_tokens=17
HTTP 200  1066ms  workbuddy:国际版:deepseek-v4.1-flash   resp=deepseek-v4.1-flash  content="收到"  total_tokens=17
HTTP 200  1472ms  workbuddy:国服:deepseek-v4.1-flash     resp=deepseek-v4.1-flash  content="收到"  total_tokens=10
HTTP 200  1710ms  workbuddy:glm-5.3                    resp=glm-5.3  reasoning_tokens=23  credit=0.01  total_tokens=48
HTTP 402    57ms  glm-5.3（裸名）          quota_exhausted: ZCode 账号没有可用额度或资源包
HTTP 402    93ms  deepseek-v4.1-flash（裸名） 同上
```

★ 裸名命中 **ZCode** 账号（无额度）⇒ 402；加 `workbuddy:` 前缀即 200。这是账号池
**按到期分层选号**的既有行为（`accountModels` 没有平台/区域维度），**不是本轮缺陷**。

### 4.2 内嵌网关逐字节验证

隔离宿主自己 materialize 出 `home\gateway\bin\gateway-4bc431-5dc57beb92aa3165.exe`，
其 SHA256 = `FD2D2BD389D3EF966BA30BF9D1AF87BC37F343A07DCD23D28429C2DBED8292EC`
= `crates/ai-gateway-core/embedded/gateway.exe`（逐字节一致）。

### 4.3 隔离环境

独立端口 `:7865`、独立数据目录 `test-bin\inst-1015\home`、**六项计划任务全部关掉**
（避免动到你的账号）。`gateway_native_config.json` 里**全部 7 个绝对路径键**逐一重写
（`listen` / `auth_dir` / `state_file` / `account_records.file` /
`pool.qoder_auth_dir` / `pool.zcode_auth_dir`），复查残留 `C:\Users` = 0 次、`:7864` = 0 次。
实测期间**未触碰**你的真实 `.wb-switch`（已核对写入方是你自己的网关 PID 49396）。

★ **踩过的坑如实记录**：我一度手工 `Start-Process` 起了 `:7865` 网关，结果前端账号池
区块**渲染为空**。根因是 `gateway_status()` 里的 `is_running()` 是个 **static AtomicBool**，
只由宿主自己 `start_gateway` 置真 ⇒ 手工起的网关对宿主**不可见**。正确做法是
`POST /api/gateway/start`。**不是前端 bug。**

---

## 五、已知问题与降级项（如实列出）

1. **Trae 授权码路径的成功分支** —— 需要一个新鲜授权码（见 2.3）。信封已证明正确。
2. **`DeviceProof` 刷新路径的成功分支** —— 需要真实 refresh token（你还没有 Trae 账号）。
3. **`ZCode` 真 JWT 的成功路径**（`preview-plan` / `claim-plan` / `warm`）—— 沿用 1.0.14 的未验证面。
4. **`Qoder` OAuth 的 `ready` 分支** —— 需人工点浏览器授权。
5. ★★ **其它 30 处 `reqwest::Client::builder()` 未逐一审计** —— 本轮只修了
   `http_request_direct` 与 `trae_refresh::exchange_token` 两处（即 Trae 链路）。
   其余（`gateway.rs` 6 处、`cli_quota`、`doubao_*`、`zcode`、`upstream\client.rs` 等）
   **可能踩同一个系统代理坑**，未动。这是**已知的、范围明确的技术债**。
6. `/usage` 的 `billingCycles` 仍无消费者 —— 沿用 1.0.14 的裁定。
7. `redisstore` 刻意未移植 —— `/status` 的 `redis_mode` 恒为 `"noop"`。

---

## 六、按你的决定**未做**的事

- **未 commit / 未 push** —— 你未要求。当前 worktree 有 **168** 项未提交改动。
- **未开新仓库** —— 这是你的决定，见交付说明里的分析。

---

## 安装

- **Windows**：`AI Gateway_1.0.15_x64-setup.exe`（NSIS）
- 升级后旧的 `gateway-*.exe` 会被自动清理，无需手工处理
- ★ `ai-gateway-server`（webui 后端）经 **npm** 分发，**不在**这个安装包里 ——
  但上面的 UI 修复全部在前端，随本包一起更新
