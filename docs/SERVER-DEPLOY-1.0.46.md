# ai-gateway 1.0.46 纯部署版（server）部署文档

> 适用产物：`dist\ai-gateway_1.0.46_server-x64.exe`
> 形态：**单文件、无 GUI 窗口、无 WebView、无安装器**，前台跑一个本地 HTTP 服务
> 编写时间：2026-10-14 ｜ 对应提交：`wt-port` HEAD `0bcea00` ｜ 版本号：1.0.46

---

## 0. 结论速览

| 项 | 值 |
|---|---|
| 交付文件 | `ai-gateway_1.0.46_server-x64.exe`（27,090,432 字节） |
| SHA256 | `5DA963036E78B20B610489DEB887E909C2682313715689D62F7AD9FD2E0E70F9` |
| 版本自证 | `ai-gateway.exe version` → `ai-gateway 1.0.46` |
| 管理界面 | `http://127.0.0.1:57890/`（默认端口，可 `--port` 改） |
| 网关端口 | `127.0.0.1:7863`（默认，可在管理界面/配置里改） |
| 数据目录 | `%USERPROFILE%\.wb-switch\`（可用环境变量 `AI_GATEWAY_HOME` 改） |
| 依赖 | Windows 10/11 x64；VC++ 运行库（`VCRUNTIME140.dll` + UCRT）；一个浏览器 |
| 数字签名 | **无**（NotSigned）→ 首次运行会有 SmartScreen「未知发布者」提示 |

**5 分钟部署（最小步骤）**

```bat
:: 1) 放进一个固定目录，双击或命令行启动
D:\APP\ai-gateway-server\ai-gateway.exe serve --port 57890

:: 2) 浏览器打开 http://127.0.0.1:57890/
:: 3) 「账号管理」页导入/添加账号（网关需要账号库非空才能启动）
:: 4) 「网关」页把 auto_start 打开，或点「启动网关」
:: 5) 客户端指向 http://127.0.0.1:7863 即可
```

---

## 1. 交付物与校验

### 1.1 文件信息

| 文件 | 大小 | SHA256 |
|---|---|---|
| `ai-gateway_1.0.46_server-x64.exe` | 27,090,432 B | `5DA963036E78B20B610489DEB887E909C2682313715689D62F7AD9FD2E0E70F9` |

构建来源：`wt-port\scripts\build-single.ps1` → 输出 `wt-port\dist-single\ai-gateway.exe`，
与 `dist` 下的交付名**逐字节一致**。

### 1.2 校验方法

```powershell
Get-FileHash .\ai-gateway_1.0.46_server-x64.exe -Algorithm SHA256
(Get-Item .\ai-gateway_1.0.46_server-x64.exe).Length
.ai-gateway_1.0.46_server-x64.exe version
```

期望：哈希与上表一致、长度 27090432、版本输出 `ai-gateway 1.0.46`。

> ⚠ **不要用文件属性里的「版本」判断本文件版本** —— 该 exe 的 PE 版本资源是空的
> （FileVersion / ProductVersion 均为空），属性页什么都看不到；也没有数字签名，
> Windows 会显示「未知发布者」。**唯一可信的版本来源是 `version` 子命令**。

---

## 2. 目标环境与前置条件

| 项 | 要求 |
|---|---|
| 操作系统 | Windows 10 / 11 x64（Server 2016+ 亦可） |
| 架构 | **x64 only**（不提供 32 位构建） |
| 运行库 | 需 `VCRUNTIME140.dll` + 通用 C 运行库（UCRT，Windows 10 起自带）。精简版系统/老系统装一次 `vc_redist.x64.exe` |
| 磁盘 | 约 200 MB（exe 27 MB + 数据目录 + 内嵌网关副本 14 MB） |
| 网络 | 出网访问上游接口（腾讯 copilot 域）；本机回环端口自用 |
| 端口 | 管理端口默认 57890；网关端口默认 7863；两者都只绑 `127.0.0.1` |
| 账号 | 至少 1 个可用账号（**账号库为空时网关拒绝启动**，返回 `账号库为空，请先添加账号再启动网关`） |

**不需要**：Node.js、npm、Rust、Python、任何数据库。前端与网关都已内嵌在 exe 里。

---

## 3. 部署形态

### 形态 A：单机默认（推荐先跑这个）

```bat
D:\APP\ai-gateway-server\ai-gateway.exe
```
等价于 `serve`：监听 `127.0.0.1:57890`，并**自动打开浏览器**。

### 形态 B：固定端口 + 不弹浏览器（长期运行/被脚本托管）

```bat
D:\APP\ai-gateway-server\ai-gateway.exe serve --port 57890 --no-open
```

### 形态 C：独立数据目录（与桌面版/其它实例互不干扰）

```bat
set "AI_GATEWAY_HOME=D:\data\ai-gateway"
D:\APP\ai-gateway-server\ai-gateway.exe serve --port 57890 --no-open
```

`AI_GATEWAY_HOME` 一旦非空，账号库、网关配置、日志全部落在该目录下，**完全隔离**。
多开实例时每个实例必须同时满足：独立 `AI_GATEWAY_HOME` + 不同 `--port` + 不同网关端口。

### 形态 D：注册为开机自启

本产品**不自带服务安装器**（刻意不写注册表/不装系统服务）。两种做法：

1. **计划任务（推荐，普通用户权限即可）**
   ```powershell
   $exe = 'D:\APP\ai-gateway-server\ai-gateway.exe'
   $act = New-ScheduledTaskAction -Execute $exe -Argument 'serve --port 57890 --no-open'
   $trg = New-ScheduledTaskTrigger -AtLogOn
   Register-ScheduledTask -TaskName 'ai-gateway-server' -Action $act -Trigger $trg -RunLevel Limited
   ```
2. **启动文件夹**：给启动参数做成快捷方式放进 `shell:startup`。

> 「服务端形态没有托盘、没有窗口，退出只能靠 Ctrl+C 或结束进程」——自启请优先选计划任务，
> 便于用 `Stop-ScheduledTask` / 任务计划程序统一停。

### 形态 E：跑在无交互的环境（CI / 远程会话）

用 `--no-open`，并**务必确认没有别的进程占用 57890**。启动即失败时会打印
`启动失败: 端口 57890 被占用或不可用（...）。可用 --port 指定其他端口。` 并退出码 1 ——
脚本里应以此判定失败。

---

## 4. 命令行接口

本二进制的命令行**很小**，全部如下（`main.rs`）：

| 命令 | 行为 |
|---|---|
| `ai-gateway.exe`（无参数） | 等同 `serve` |
| `ai-gateway.exe serve` | 启动本地服务；可选 `--port <n>`、`--no-open` |
| `ai-gateway.exe version` / `--version` / `-V` | 打印 `ai-gateway <版本>` 后退出 |
| `ai-gateway.exe status` | 打印版本、WorkBuddy 是否运行中、当前账号、账号数后退出 |

- **没有 `--help`、没有 `--hidden`、没有 `--daemon`。**
- ⚠ 未知参数**不会报错**，会被当作 `serve` 处理（例如 `--hidden` 只是被忽略，进程照常对外服务）。
  请勿依赖未知开关表达意图。
- 端口优先级：`--port` 指定值 > 默认 57890。`--port` 后面必须跟合法数字，否则回落默认值。

### 启动成功的输出（stdout，原样）

```
ai-gateway v1.0.46
webui: http://127.0.0.1:57890
按 Ctrl+C 停止服务。
```

出现 `webui:` 这行即代表服务已就绪；巡检脚本可以据此判断启动成功。
停止：`Ctrl+C`（前台）或结束该进程（后台）。

---

## 5. 数据目录布局

默认根目录 `%USERPROFILE%\.wb-switch\`（即 `C:\Users\<你>\.wb-switch\`）；
设了 `AI_GATEWAY_HOME` 则用它。**首次全新启动只会创建一个文件**（`codebuddy_cn_app.json`），
其余目录在用到时按需创建。

| 路径 | 内容 | 说明 |
|---|---|---|
| `accounts.json` | 账号库 | **唯一真源**，含可登录凭证，务必单独备份、勿分享 |
| `codebuddy_cn_app.json` | 本机标识 | 首次启动即创建 |
| `gateway\gateway_config.json` | 网关**管理侧**配置 | 端口、API Key、模式、各任务开关、守栏杆位 |
| `gateway\gateway_native_config.json` | 网关**进程侧**配置 | 由上一份**派生**，不建议手改（见 §6.3） |
| `gateway\gateway_auths\` | 网关凭证 | 从账号库派生；删掉会自动重建 |
| `gateway\bin\gateway-<内容指纹>.exe` | 内嵌网关副本 | 首次启动网关时释放，按内容指纹命名，升级自动换新 |
| `gateway\gateway_data\state.json` | 网关运行态 | 账号档位/熔断等 |
| `gateway\gateway_data\usage.json` | 用量计数 | 每账号每日 `input/output/cacheWrite`，护栏据此判定 |
| `gateway\gateway_data\gateway.log` | 网关日志 | 排障第一现场 |
| `auto_checkin_logs.json` 等日志 | 签到/轮换等日志 | 按「记录保留天数」清理（默认 60 天，设置页可改） |

> 备份/迁移 = 打包整个数据根目录。换机器只要账号库跟着走，网关配置与凭证都会自动重建。

---

## 6. 配置

### 6.1 管理侧：`gateway\gateway_config.json`

界面「网关」页改的就是这份。常用字段：

| 字段 | 默认 | 说明 |
|---|---|---|
| `port` | `7863` | **网关端口（权威字段）**，`listen` 由它派生 |
| `listen` | `:7863` | 绑定地址串；同一份配置里**以 `port` 为准**，`listen` 会被归一化 |
| `api_key` | 空 | 网关鉴权密钥；**空 = 完全不鉴权** |
| `mode` | `balance` | `balance` 负载均衡 / `pinned` 指定账号 |
| `pinned_uid` | `null` | `pinned` 模式锁定的账号 uid |
| `auto_start` | `false` | 服务/应用启动后**自动拉起网关**（账号库非空时生效，约 7 秒起来） |
| `enabled` | `false` | 网关总开关状态 |
| `daily_token_cap` | `0` | 账号级**每日 token 上限**；`0` = 关闭（1.0.46 新增） |
| `credit_floor` | `0` | 账号级**保留积分下限**；`0` = 关闭（1.0.46 新增） |
| `checkin_enabled` / `keepalive_enabled` / `activity_enabled` / `nightowl_enabled` / `school_enabled` / `trial_enabled` | `true` | 各类自动任务开关（缺省全开） |
| `activity_hours` / `nightowl_hours` / `school_hours` / `trial_hours` | `[10]`/`[1]`/`[12]`/`[9,21]` | 任务时点（北京时间） |
| `activity_report_count` | `3` | 每号每日活跃上报条数（界面输入框上限 20） |

> 养号排程类改动需要**重启网关**才生效；守栏杆位（`daily_token_cap` / `credit_floor`）是
> **5 秒热加载**，无需重启（见 §9.4）。
> 本文件手改后也要**重启网关**才生效（字段是启动时读入的）。

### 6.2 进程侧：`gateway\gateway_native_config.json`

由管理侧配置转换而来，额外可手调：

| 字段 | 默认 | 说明 |
|---|---|---|
| `schedule.checkin_scope` | `"cn"` | 签到/旅行范围；`"all"` 含国际版 |
| `pool.credit_refresh_interval` | `"15m"` | 积分到期巡检周期 |
| `pool.credit_refresh_enabled` | `true` | 是否启用积分巡检 |
| `pool.idle_weight_per_hour` | `0.5` | 档内权重的闲置补偿 |
| `pool.idle_weight_max` | `5.0` | 闲置补偿封顶 |
| `pool.max_in_flight` | `3` | 单账号在途请求上限 |
| `pool.breaker_threshold` | `3` | 连续失败几次触发熔断 |

### 6.3 ⚠ 手改的注意事项

- `gateway_native_config.json` 是**派生文件**：管理侧一保存/重启就会被重写，手改会丢。
  要持久化请改 `gateway_config.json` 或走界面。
- `daily_token_cap` / `credit_floor` **必须写在原生配置的顶层**。写进 `pool` 里不会报错，
  但会被静默忽略（等于没配）。

---

## 7. 首次使用：把账号加进去

1. 浏览器打开 `http://127.0.0.1:57890/`（自动进入「账号管理」页）。
2. 三种导入方式任选：
   - **本地扫描导入**：`GET /api/import-local/scan` 对应的界面按钮，扫本机 CodeBuddy 登录态；
   - **短信登录**：界面上发码/验证码登录；
   - **批量导入**：粘贴/导入账号 JSON（支持预览后再入库）。
3. 账号库非空后，把「网关」页的 `auto_start` 打开（或直接点「启动网关」）。

> ★ **启动服务 ≠ 启动网关**。`ai-gateway.exe serve` 只拉起管理端；网关（真正提供
> `/v1/chat/completions` 的进程）要么点「启动网关」，要么 `auto_start=true` 时自动拉起。
> 已验证：账号库有 2 个账号 + `auto_start=true` 时，**服务启动后约 7 秒网关自行起来并开始监听**；
> 结束服务进程时网关**随之退出，不会留孤儿进程**。

---

## 8. 客户端接入

网关就绪后（默认 `http://127.0.0.1:7863`）：

### 8.1 OpenAI 兼容（任意 OpenAI SDK / 客户端）

```bash
export OPENAI_BASE_URL=http://127.0.0.1:7863/v1
export OPENAI_API_KEY=<你在网关配置里设置的 api_key>
```

```bash
curl http://127.0.0.1:7863/v1/models -H "Authorization: Bearer $OPENAI_API_KEY"

curl http://127.0.0.1:7863/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}],"stream":false}'
```

### 8.2 Anthropic Messages（Claude Code / Claude Desktop）

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:7863
export ANTHROPIC_AUTH_TOKEN=<你的 api_key>
```

> 界面「智能体管理」页可一键写入上述配置（含模型与认证，写前自动备份）。

### 8.3 网关对外路径一览

| 方法与路径 | 用途 |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions（含流式） |
| `POST /v1/responses`（也支持 `/responses`） | Responses 协议 |
| `POST /v1/messages`（也支持 `/messages`） | Anthropic Messages 协议 |
| `GET /v1/models` | 可用模型列表 |
| `GET /v1/models/regions` | 按区域的模型能力真值 |
| `GET /status`、`GET /healthz` | 探活/状态 |
| `GET /usage` | 用量报表 |
| `POST /tasks/run`、`POST /tasks/growth` | 手动触发任务 |
| `GET /debug/`、`GET /debug/capability`、`POST /debug/accounts/:uid/revive` | 诊断/复活账号 |

鉴权：优先读 `Authorization: Bearer <key>`，其次 `x-api-key`；**`api_key` 为空时完全不校验**，
错误码 `invalid_api_key`。

---

## 9. 常用运维操作

### 9.1 用 HTTP 管（推荐脚本化）

```bash
# 服务状态
curl http://127.0.0.1:57890/api/status

# 网关状态（running / enabled / accounts / base）
curl http://127.0.0.1:57890/api/gateway/status

# 启动 / 停止 / 重启网关
curl -X POST http://127.0.0.1:57890/api/gateway/start
curl -X POST http://127.0.0.1:57890/api/gateway/stop
curl -X POST http://127.0.0.1:57890/api/gateway/restart

# 端口占用体检（返回 available / holder / suggest）
curl -X POST http://127.0.0.1:57890/api/gateway/port-check -H "Content-Type: application/json" -d '{"port":7863}'

# 读 / 写网关配置
curl http://127.0.0.1:57890/api/gateway/config
curl -X POST http://127.0.0.1:57890/api/gateway/config -H "Content-Type: application/json" -d '{"port":7863,"api_key":"sk-xxx","mode":"balance","auto_start":true}'
```

> 管理端口**没有鉴权**，但只绑 `127.0.0.1`。任何本机进程都能调它的写接口 —— 单机部署请
> 按「本机可信」假设使用，不要把 57890 暴露出去。

> ⚠ **两种改配置的写法，生效时机不同**（本形态/服务端**没有 Tauri 命令层**，只有 HTTP）：
>
> | 改动方式 | 落盘 | 生效 |
> |---|---|---|
> | 界面「网关」页保存 / `POST /api/gateway/config`（**只改栏位**） | 会 | **最长 5 秒热加载**，不重启网关（守栏杆位就是这么生效的） |
> | 界面点「启动网关」/ `POST /api/gateway/start`、或 `POST …/mode` `…/restart` | 会 | 立即按新配置重起网关进程 |
> | 手改 `gateway_native_config.json` | 会 | **最长 5 秒热加载**（但那文件会被管理侧覆盖，见 §6.3） |
>
> 也就是说：**改了配置后不必重启网关**；重启只在「端口这类只在启动时绑定的项」上才必要（§9.2）。
> 顺带一条运维口径：`running` 与 `enabled` 是**两个不同字段** —— 网关正在跑时 `running=true`，
> 而 `enabled` 只是开关状态，实测自动拉起后它仍可能是 `false`。**判断存活看 `running` / 端口 / `/healthz`。**

### 9.2 端口被占用

```powershell
# 看谁占着
curl -X POST http://127.0.0.1:57890/api/gateway/port-holder -H "Content-Type: application/json" -d '{"port":7863}'
# 或本地查
Get-NetTCPConnection -LocalPort 7863 -State Listen | Select-Object OwningProcess
```
界面提供「结束占用进程」（`POST /api/gateway/port-kill`）。另一种做法是换端口：
改 `port` 后**重启网关**（端口在网关启动时绑定，改完只有重启才生效）。

### 9.3 升级

1. 停服务（Ctrl+C 或结束进程；网关会随服务一起退出）；
2. 用新 exe 覆盖旧文件（**数据目录不动**）；
3. 重新启动。

数据、账号、配置全部保留；内嵌网关副本按内容指纹自动换新，旧副本可留可删。

### 9.4 账号级用量护栏（1.0.46 新增）

两个门槛都在 `gateway_config.json`：

| 栏位 | 作用 | 语义要点 |
|---|---|---|
| `daily_token_cap` | 今日 token 数超过该值 → 不再选它 | 计数 = `input + output + cacheWrite`（**不含缓存读取**）；「今日」按**本机本地日期**，**零点自动恢复** |
| `credit_floor` | 剩余积分低于该值 → 不再选它 | 积分由巡检刷新；**与日期无关，不会零点恢复**（巡检回充或调低下限才会解除） |

行为特征（实测）：

- 命中后该账号返回 **HTTP 429**，错误码 `rate_limit_exceeded`，报文会写明原因与恢复途径；
- 账号**仍在列表中、不会被禁用**，只是不参与选号；
- 是**选号闸门**不是限流器：用量在请求结束后记账，最多有约 5 秒滞后，**不会中断在途请求**；
- 改配置后**最长 5 秒热加载生效**，不需要重启网关；
- 同时命中两个栏位时**额度原因优先**。

自查是否被护栏挡住：`GET /api/gateway/status` 里对应账号的 `daily_guard` 为 `true`；
`gateway\gateway_data\usage.json` 可看到当日计数。

### 9.5 日志

- 网关日志：`<数据目录>\gateway\gateway_data\gateway.log`（排障第一现场）
- 服务端 stdout/stderr：前台直接看；后台运行请自行重定向到文件
- 界面内也可查看（`GET /api/gateway/log`）

---

## 10. 安全边界

| 项 | 事实 |
|---|---|
| 监听范围 | 管理端与网关**都只绑 `127.0.0.1`**，局域网/外网默认访问不到 |
| 管理端鉴权 | **无**（仅依赖回环地址保护） |
| 网关鉴权 | 由 `api_key` 决定；**留空即无鉴权** —— 只要本机能连 7863 就能白嫖你的额度 |
| 数字签名 | 无。企业环境如启用「仅允许签名程序」，需要自行加白名单 |
| 凭证 | `accounts.json`、`gateway_auths\` 含可直接登录的凭证，等同密码，勿分享/勿入版本库 |

**不要把 57890/7863 暴露到公网。** 本产品没有为对外暴露做任何加固（无 TLS、管理端无鉴权、
未按跨域场景设计）。确有远程需求，请用 SSH 隧道/RDP 之类的通道，而不是改监听地址。

---

## 11. 自检清单（部署后逐条确认）

| # | 检查 | 期望 |
|---|---|---|
| 1 | `Get-FileHash` | 等于 §1.1 的 SHA256 |
| 2 | `ai-gateway.exe version` | `ai-gateway 1.0.46` |
| 3 | stdout | 出现 `webui: http://127.0.0.1:57890` |
| 4 | `Get-NetTCPConnection -LocalPort 57890 -State Listen` | 有记录 |
| 5 | `GET /api/status` | HTTP 200 |
| 6 | `GET /api/accounts` | HTTP 200 且能看到账号 |
| 7 | 网关已起（界面或 `auto_start`） | `GET /api/gateway/status` → `running=true` |
| 8 | `GET /v1/models`（带 api_key） | HTTP 200，含模型列表 |
| 9 | `GET /v1/models`（不带 api_key，且已设 api_key） | HTTP **401** |
| 10 | 发一次真实对话 | HTTP 200，返回内容 |
| 11 | `gateway\gateway_data\usage.json` | 出现该账号当日计数 |
| 12 | `gateway\gateway_data\gateway.log` | 存在且有内容 |
| 13 | 结束服务进程后 | 网关进程随之消失，7863 端口释放 |

> 本清单第 3–13 项**已由真实运行验证**：`test-bin\deploy-verify.ps1` 对
> `ai-gateway_1.0.46_server-x64.exe` 做了 9 个阶段（交付物哈希 → 假上游 → 启动 → 内嵌网关
> 释放与哈希比对 → 管理端/网关各接口 → 真实对话 → 两个守栏杆位实测 → 数据落盘 → 清理），
> 结果 **58 PASS / 0 FAIL**（记录：`test-bin\deploy-verify.log`）。
> 其中守栏杆位是**真跑出来的**：注入 `daily_token_cap=4` → 对话被 429 拦下 → 归零后立刻放行；
> 注入 `credit_floor=200000` → 429 且报文含积分措辞 → 降到 1 后放行；全程网关进程号不变（热加载）。

---

## 12. 故障排查

| 现象 | 原因 / 处理 |
|---|---|
| 启动即 `启动失败: 端口 ... 被占用或不可用`，退出码 1 | 57890 被占。`--port` 换端口，或先找出占用者 |
| 命令行报「版本」看不到，属性页空白 | 正常。用 `version` 子命令 |
| 首次运行弹 SmartScreen「未知发布者」 | 未签名导致。点「更多信息 → 仍要运行」 |
| 打不开管理页 | 服务没起（看 stdout 有没有 `webui:` 行），或端口不是 57890（看你传的 `--port`） |
| 管理页能开，但客户端 401 `invalid_api_key` | 网关设了 `api_key`，客户端没带或带错 |
| 客户端 429 / `rate_limit_exceeded` | 命中 `daily_token_cap`（次日零点恢复）或 `credit_floor`（等积分巡检回充，或调低下限）。报文里写明了是哪一种 |
| 「启动网关」返回 `账号库为空，请先添加账号再启动网关` | 先导入账号 |
| 网关一直起不来 | 看 `gateway\gateway_data\gateway.log`；确认网关端口没被别人占；确认账号未全部过期 |
| 改了配置不生效 | 守栏杆位 5 秒热加载；**端口/任务开关需重启网关** |
| 网关配置改了没用（`daily_token_cap`/`credit_floor`） | 检查是否被写进了原生配置的 `pool` 里 —— 必须在**顶层** |
| 关不掉 / 感觉有残留进程 | 结束**服务端**进程即可，网关在同一个作业对象里会一起退出；必要时按「进程 ID + 路径」双条件确认后再结束 |
| 内嵌网关副本占空间 | `gateway\bin\` 里按内容指纹命名，同一版本只有一个；删除后下次启动会重新释放 |

---

## 13. 本形态**不包含**什么

| 不包含 | 说明 |
|---|---|
| 桌面 GUI 窗口 / 托盘 / 自动更新 | 那些在安装版（`AI Gateway_1.0.46_x64-setup.exe`，NSIS 安装器）里；本形态是纯服务端 |
| 服务安装器 / 注册表项 | 刻意不做；自启见 §3 形态 D |
| 数字签名 | 与安装版一样未签名 |
| 32 位 / ARM 构建 | 仅 x64 |
| 对外暴露的加固（TLS、多用户、鉴权隔离） | 见 §10 |

---

## 14. 从源码构建（需要自建时）

```powershell
cd <仓库>\wt-port
.\scripts\build-single.ps1                      # 全流程：网关 → 前端 → 服务端 → 拷贝到 dist-single
.\scripts\build-single.ps1 -SkipFrontend        # 跳过前端（前端产物已是最新时）
.\scripts\build-single.ps1 -SkipGateway         # 复用已构建的网关
```

产物：`dist-single\ai-gateway.exe`。脚本自带的约束：

- 拒绝把输出目录设为仓库的 `dist\`（避免覆盖交付目录）；
- 大于 30 MB 会告警；
- 结束时打印管理界面地址。

依赖：Rust 工具链（`cargo`）、Node.js + npm（前端）。首次构建需要网络拉依赖
（仓库已配 crates.io 国内镜像 `sparse+https://rsproxy.cn/index/`）。
`release.yml` 里**没有**服务端单文件的构建步骤，该产物一直是手工用本脚本产出的。

---

## 15. 一句话总结

解压即跑的单文件服务端：**双击 → 浏览器导入账号 → 网关自动起来 → 客户端指向 `127.0.0.1:7863`**。
数据全在同级 `.wb-switch` 目录，升级只换 exe，未签名是本形态最大的使用摩擦点。
