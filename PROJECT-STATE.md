# WorkBuddy Switch / AI Gateway — 项目状态与后续工作

> 最后更新：2026-10-04（**1.0.20**：①–⑩ 全量修复 —— 含 ⑨ Trae 是否已签到看不见、⑩ 启动期卡顿 / Trae 官方 icon）
> 本文件是**接手用**的持久状态说明，不随会话消失。

---

## 一、这是什么项目

`D:\WishProject\WorkbuddySwitchAPi\wt-port` —— WorkBuddy（CodeBuddy）多账号切换与 API 网关。

| 组件 | 位置 | 语言 |
|---|---|---|
| 宿主 GUI | `src-tauri/` + `src/` | Rust/Tauri + React |
| 宿主逻辑库 | `crates/ai-gateway-core/` | Rust |
| WebUI 后端 | `crates/ai-gateway-server/` | Rust |
| **网关** | `crates/ai-gateway-router/` | **Rust（2026-10 从 Go 全量重写）** |
| Go 参照实现 | `go-gateway/` | Go（**保留作对照，构建链已不依赖**） |

- git 根 = `wt-port\`，分支 `migrate/aigw-v1.0.2`
- 当前版本号 **1.0.20**（`tauri.conf.json` / `package.json` / `Cargo.toml` / `npm/` 五个平台包已对齐；签名安装包已产出并解包核验）
  - 上一个已发布版本 **1.0.19**（已被业主安装：`HKCU:\...\Uninstall\AI Gateway` ⇒ `DisplayVersion=1.0.19`、`InstallLocation=D:\APP\AI Gateway`）
  - ★ NSIS 同版本**不覆盖**（`installer.nsi:233-259` 用 `nsis_tauri_utils::SemverCompare` 比 `DisplayVersion`，相等走维护页）⇒ 升版本号是交付的硬前提

---

## 二、当前进展（2026-10-04）

### 已完成的四组重点

| 组 | 内容 | 状态 |
|---|---|---|
| **P0#1** | CLI 子命令整个缺失（会让界面卡死） | ✅ qoder-login 10 + zcode-login 8 = **18/18 真机验证** |
| **P0#2** | 三处启动接线（`product_tasks_runner` / `claim_scheduler` / `sync_to_dir`） | ✅ 真机验证 |
| **批次3 N4** | Qoder 计费周期统计（三段：存储 / 取数 / 接线） | ✅ 真机端到端（真实账号） |
| **并发缺陷** | 客户端断开时账号锁泄漏 | ✅ 150.4s 全挂 → 5.0s 通过 |
| **代理泄漏** | reqwest 偷偷走 Windows 系统代理 | ✅ 见下方「陷阱」 |
| **1.0.17 A** | Trae 进「智能体管理」（写 `state.vscdb` 自定义模型表） | ✅ API + UI 双半段真机实测，闭环 `HTTP 200` |
| **1.0.17 B** | Trae 进「兼容网关」（`products\trae\` 适配器，约 5196 行） | ✅ 真账号 4 模型 `200` + SSE；抓到了默认产品线判错的根因 |
| **11133 修复** | 业主对话里的 400 `code=11133`（空名 tool-call 毁掉整会话） | ✅ 根因闭环 + 三层修复 + 独立实例 6 条畸形探针 400→200（业主 :7864 同组 6/6 全 400 对照）；并用业主那条真实 id `call_120c4dae9cdc41168ec0cc39` 1:1 复现 400 → 修复版 200 |
| **11133 复测** | 用**交付包内嵌网关**（`test-bin\inst-pkg-final\`，端口 7922）原样重打同一批探针 | ✅ 9 探针 A/B：业主 `:7864` 8/8 全 400；交付包网关 8/8 全 200 + `PONG`（流式 5 帧 + `[DONE]`）；整流日志 8 行逐条对账 |
| **1.0.20 ①–⑩** | 业主回退 GitHub 发布版后报的全部问题，一次修完（详见 `test-bin\DELIVERY-1.0.20.md`） | ✅ 全部落盘 + 交付包本体独立实例实测（`test-bin\inst-pkg-1020\`，`:57893`） |
| ① Trae 详情页 404 | webui 缺 3 条路由（桌面端 IPC 本来就有）⇒ 补 `api.rs:319/:341/:357` | ✅ `:57893` 实测 detail/history/stats 200；缺参 ⇒ 400 |
| ② 裸名路由到 Trae | `bin/gateway.rs:154-164` 注入 Trae 兜底清单（58 模型） | ⚠ **本机 A/B 未能复现差异**（1.0.19 里这段已存在）⇒ 只能用源码 + 单测 + Trae-only 池对照证明 |
| ③ 用量页裂行 / 成本优先 | `forward.rs:390 inject_cost_rates` + `capability.rs:1341-1434` | ✅ `/api/gateway/models` 210 条，`deepseek-v4.1-flash` 只 1 条（无 trae 通道） |
| ④ Trae 积分 0 / 到期未知 / 正在被使用 | 新增 `products/trae/credits.rs` + `trae-credit-cache.ts:79` | ✅ 交付包侧实测 `credits:667` / `expire_day:2026-10-31` / `in_flight:0` |
| ⑤ 会话中断 | `server.rs:2712 chat_stream_error_frame`，恰一个 `[DONE]` 前补错误帧 | ✅ 假上游 `abort` 模式复现后客户端正常收敛 |
| ⑥ 宿主假 GATEWAY_RUNNING（⑦） | Plan A/B/C/D：删静态布尔 + `/healthz` 认领 + 中文提示 + `stop_gateway()` | ✅ `:57893` 实测 port-check / port-holder / stop 全 200 |
| ⑦ webui API_BASE 硬编码（⑧） | `api.ts:89 resolveApiBase()`（`VITE_API_BASE` > `location.origin` > 默认） | ✅ 交付包前端在 `:57893` 上 45/45 请求都指向自己的源，含 57890 的 URL 0 个 |
| ⑧ Trae 是否已签到看不见（⑨，m13266） | `record_probe_today` 与 `record_today` 分离，探针**永不覆盖**真实 `fail` | ✅ 种 `fail` 后点刷新，JSON **逐字节未变**；UI 三态「已签到/未签到/未确认」 |
| ⑨ Trae 卡片官方 icon（⑩-1） | 两张官方 PNG + `product-marks.tsx:144-163 TraeMark(variant)` | ✅ 交付包前端渲染 5 张 trae 图，全部 512×512、`size-full object-cover` |
| ⑩ 启动期卡顿（⑩-2，m13819 + m14110） | 前端限流 4 并发 + `/api/credits/batch` + `warm_caches()` + `CHECKIN_STATUS_CONCURRENCY=6` | ✅ A/B：API **167 ⇒ 45**、最慢 **27,412 ms ⇒ 2,717 ms**、longtask **0** |

### 交付物

```
dist\AI Gateway_1.0.20_x64-setup.exe   14,406,438 B   （2026-10-04 02:50:31 UTC）
SHA256 691454CB0096A9116D7FBC963C1ED6874C0A4FD964C01C263EA2E2AD0E2A1DD6
  .sig  424 B  SHA256 F307273CCAFFE6D53037602CFE2C1A2104977B2A6E0E442B9F3E8B324B77DD22  keyid 7e7ce64b6fc3a36f
内嵌网关 13,684,224 B  SHA256 43C9E48DFA27DAA9B5BE92A2D4AD5424F41534E203ADD383B573AC4A81E86388
（已 7-Zip 独立解包 + 从包内 GUI exe raw-inflate 出网关并与 embedded/gateway.exe 逐字节比对通过）

★ 历史（1.0.17）：
dist\AI Gateway_1.0.17_x64-setup.exe   14,311,130 B   （含 11133 修复，2026-10-03 18:33 重打）
SHA256 8331371362744B4A22D86F7E8C8974AEB33F7B77EC896BD39C0D4DF767701662
  .sig  424 B  SHA256 F9D28C144AABB8001892B5DD7AD655E48E824675C8FC4C339AA77A04A21F7DF9
内嵌网关 13,619,712 B  SHA256 D8CE65557A5FC4A6F489BAAFAB0775E6CA3399247E997F5D4F67EEEDA7302105

★ 2026-10-03 第二轮（11133 修复）：网关源码改了 3 个文件
  （`products/trae/translate.rs` / `upstream/payload.rs` / `protocol/rectify.rs`），
  embedded/gateway.exe 与安装包都已按同一条链重建并签名。
  上一轮（无修复）的安装包：14,309,063 B
  SHA256 4CBF382F925D9FD40F905707E313141BBA580A28E20FEB09464272A3C152F10A
```

★ 权威文档：
- 发布说明 `.github\RELEASE_NOTES_1.0.20.md`
- 交付汇总 **`test-bin\DELIVERY-1.0.20.md`**（701 行，含全部实测数字与「仍未验证面」）
- 历史：`.github\RELEASE_NOTES_1.0.17.md` / `test-bin\DELIVERY-1.0.17.md`

★ 注：Rust release 构建**不确定**（同一份源码两次构建 SHA256 不同，实测确认）
⇒ 网关指纹**只在同一次构建链内可比**，跨次构建不能按哈希比对。

---

## 三、测试基线（改动后必须不低于这些数）

| 命令 | 基线 |
|---|---|
| `cargo test -p ai-gateway-router --lib --offline` | **1633 passed / 0 failed**（1.0.20；11133 后是 1603） |
| `cargo test -p ai-gateway-core --lib --offline` | **1235 passed / 0 failed / 4 ignored**（58.59 s）（1.0.17 时是 1212 / 133.82 s） |
| `cargo test -p ai-gateway-server --offline` | **28 passed / 0 failed** |
| `cargo test --workspace --offline`（15 个 test 块累加） | **2943 passed / 0 failed / 5 ignored / 0 errors**（1.0.20，`test-bin\workspace-test-1020.log`，EXIT=0；1.0.17 时是 2882） |
| Trae 相关（`products::trae::` 128 + `modules::trae_agent::` 38 + `modules::trae_account::` 22） | **188 passed** |
| `cargo test --workspace --lib --offline`（1.0.14 时的旧口径） | 2619 passed / 0 failed |
| `cargo check -p ai-gateway-router --lib --offline` | EXIT 0，**0 warnings** |
| `cargo check -p ai-gateway-router --bin gateway --offline` | EXIT 0，**0 warnings** |
| `cargo check --workspace --all-targets --profile test --offline` | EXIT 0 |

★ `cargo` 不在默认 PATH：
```powershell
$env:PATH = "C:\Users\iuuuuuuuu\.cargo\bin;$env:PATH"
```
★ crates.io 不可达 ⇒ 所有 cargo 命令加 `--offline`。

---

## 四、★★ 本仓库特有的陷阱（血泪，务必先读）

### 1. `cargo test --workspace` 与 `-p` **不是同一个构建**
`src-tauri` 的 `tauri-plugin-updater` 拉 reqwest 0.13.4，会打开
`hyper-util/client-proxy-system`。而 `hyper-util` 全 workspace 只有一个版本 ⇒
**Cargo 按包版本统一 feature** ⇒ 我们自己的 reqwest 0.12.28 在 `--workspace`
构建里**也**拿到系统代理能力，`-p` 构建里没有。

后果：reqwest 默认 `auto_sys_proxy=true`，会读 **Windows 注册表**的系统代理
（Go 的 `http.ProxyFromEnvironment` 只读环境变量）⇒ 「没配代理」在 Go 是真直连、
在 reqwest 是偷偷走代理。

**修法**：未配置代理时必须显式 `.no_proxy()`（`upstream/client.rs` 的 `match proxy`）。

### 2. ★★ `Copy-Item` 保留 mtime ⇒ cargo 跳过重建 ⇒ 测到旧二进制（**假绿**）
做 A/B 还原验证时**必须**：
```powershell
(Get-Item <file>).LastWriteTime = Get-Date
```
否则「还原后测试仍通过」是假象（它跑的是上一轮的二进制）。这个坑我踩过两次。

### 3. `serde_json` 无 `preserve_order` ⇒ `Map` 是 `BTreeMap`
`json!({...})` 会按键的**字典序**重排字段。凡「字段顺序参与签名」的上游请求体，
**必须**用 `#[derive(Serialize)] struct` 按声明序序列化。这类错误**不报错**，
只让上游签名校验失败。

### 4. 网关 `main()` **没有** `--version` 分支
`gw.exe --version` 会被当成「无 config 启动」⇒ **起 HTTP 服务并常驻**。
所以**绝不要**直接运行共享目录里的可执行文件（哪怕只是读版本号）；
要跑就先 `Copy-Item` 到自己的目录。

★ 2026-10-04 修正一条旧说法：**NSIS 模板确实会按进程名杀**。`installer.nsi:641` 与 `:768` 两处 `!insertmacro CheckIfAppIsRunning "ai-gateway.exe" "AI Gateway"`，
宏体在 `target\release\nsis\x64\utils.nsh`，内部走 `nsis_tauri_utils::FindProcessCurrentUser` + `KillProcessCurrentUser`（**按可执行文件名**）。
所以「安装前先退出桌面版」是必须的提醒，而且这也**再次确认了「绝不运行安装包」这条规矩**。
（另注：业主的网关进程名是 `gateway-4e8a56-9affe6e7f02852f9.exe`，名字不同，安装器不会碰它。）

### 5. `bin/gateway.rs` 是**独立 crate**
看不到 lib 的 `pub(crate)` 项。所以需要 `pub(crate)` 依赖的逻辑
（如 `claim_all_campaigns`、`QoderClient`）**必须**在 lib 内做成 `pub` 入口，
bin 只调用它。范例：`products/product_tasks.rs`、`products/qoder/billing_cycle.rs`。

### 6. 其它
- `Select-String` 单引号里的 `\"` 是**字面量** ⇒ 会假阴性；`^\s*(pub )?mod ` 漏 `pub(crate) mod`
- ★★ **审计「定义 vs 注册」清单时别用 `-Context 0,N`** —— 它会在 N 行处**静默截断**，
  长清单被少算。我据此误报「10 个 Tauri 命令未注册」，实际 86=86 零缺口。
  正解：`ReadAllLines()` 定位块起止行，在**完整行区间**上迭代正则。
- ★★ **本仓库的 `.rs` 是 CRLF**（`api.rs` 实测 2943 CRLF / 0 裸 LF）⇒ 跨行匹配必须先
  `replace("\r\n","\n")`，否则含 `\n` 的模式**永远匹配不上**（假红）。
- `Get-Content` 把 UTF-8-no-BOM 的 `.rs` 读成 GBK ⇒ 用 `read` 工具；
  数行数用 `[System.IO.File]::ReadAllLines($p).Length`
- `cd D:\...` **不改变 .NET 进程 CWD** ⇒ `[System.IO.File]` 必须用绝对路径
- 假上游用 **`ThreadingHTTPServer` + `daemon_threads`**；单线程会让网关死锁
- axum 在客户端断开时**丢弃 handler future** ⇒ 清理逻辑必须用 RAII 守卫
- ★ **`ai-gateway-server` 不在 NSIS 安装包里** —— 它经 **npm** 分发
  （`npm/platform/ai-gateway-win32-x64`）。改它**不需要**重打 `dist\*.exe`。
- ★ **起宿主服务必须设 `AI_GATEWAY_HOME` + `--port`**：不设它会用**真实数据目录**、
  还会触发网关重启（我踩过一次：把所有者 `:7864` 的 pid 从 24900 换成了 42576）。
  正确做法：独立目录 + `serve --port 5799x --no-open`，并验证目录里**只有日志与 exe**。

---

## 五、已知未验证面（如实记录）

| 项 | 状态 |
|---|---|
| Trae **客户端里真点一次对话** | ❌ 未做（我不主动拉起你的 IDE）。已实测到「按库里写下的 `base_url` + 解出的凭据真发请求 ⇒ 200 + 真实回答」 |
| Trae `custom_model_id` 是否被客户端本地校验 | ❓ 未证（代码优先复用库里已有值，实测复用到 `2005145218`，不编造） |
| Trae `sk` 字段（`auth_type:1` 场景） | ❌ 无样本（库里 3 条活动记录全是 `auth_type:0` / `sk:""`） |
| ZCode **真 JWT 的成功路径** | ❌ 未跑通（`preview-plan` / `claim-plan` / `warm` 的成功分支）。需真实 ZCode JWT |
| Qoder OAuth `ready` 分支 | ❌ 未在真机走到（需人工点浏览器授权） |
| DST 异常时区的月步进 | ❌ 未在本机复现（依赖真实 tzdata 瞬间）；防御逻辑已实现并测试 |
| 11133 的 **Trae 侧修复**（丢弃空名 tool-call） | ❌ 无法用真实上游复现 —— 实测 Trae 既不吐结构化 `tool_calls`、也不校验 tool 配对（污染历史探针返回 200，整流日志 0 命中）⇒ 只有单测覆盖（2 条） |
| 11133 修复版**部署到所有者 `:7864`** | ❌ 未部署（那台仍是 1.0.19 血统）⇒ 1.0.20 安装后即覆盖 |
| **GUI 侧实测**（托盘 / 单实例 / 自动更新 / 开机自启） | ❌ **设计上做不了**：桌面版第一个注册的插件是 `tauri-plugin-single-instance`，互斥量 `com.momo0410.aigateway-sim`；只要业主的 `D:\APP\AI Gateway\ai-gateway.exe` 在跑，我启动解包出来的 GUI exe 只会 `exit(0)` 并聚焦他的窗口，预热/托管/托盘全不跑 |
| **Trae SG / 国际版分支** | ❌ 无真实 SG 账号可测，只有单测覆盖（业主账号 JWT 无 `iss` ⇒ `RealmEvidence::Default` ⇒ 界面「国内版（推断）」） |
| ② 裸名路由的**本机 A/B 复现** | ⚠ 未能复现差异（旧/新网关对裸名都返回 WorkBuddy）—— 该兜底清单在 1.0.19 已存在；只能靠源码 + 单测 + Trae-only 池对照 |
| `ai-gateway-server` 启动横幅版本串 | ⚠ 仍打印 `ai-gateway v1.0.19`（该 crate 自己的版本常量没跟升），**不影响功能**，已写进交付说明第四节 |
| 11133 修复的**覆盖边界** | ⚠ `prepare_body` 只覆盖默认上游（workbuddy/cmb 族）与 Qoder；**Trae / ZCode 各有独立适配器、不经此处**。Trae 已在源头修掉；**ZCode 侧未修**（`products/zcode/translate.rs:157` 同样 `unwrap_or("")`），但其上游是 Anthropic 形状（不回 11133）、池里只有 2 个模型、会话里无 ZCode 流量 ⇒ 已知残余风险 |

---

## 六、硬性规矩（违反会伤到所有者）

- Shell **必须** PowerShell；路径用 `\`
- **绝不运行安装包 / 卸载器** —— NSIS 按**可执行文件名**杀进程，
  而主程序 `ai-gateway.exe` 与所有者正在跑的实例同名，会连带打掉 `:7864`
- **绝不按进程名杀进程** —— 只允许 **PID + ExecutablePath 双条件**
- **绝不运行**共享目录里的可执行文件（**哪怕 `--version`**）
- **绝不 commit / push / 提 PR**，除非所有者明确要求
- 所有者服务 `:7864`（网关）/ `:3080`（DSH GUI）**必须保持在线**
- 公开仓库 ⇒ 被 git 跟踪的文件里**不得有真实 token / UID / deviceMid**
- 改完**必须自己跑实测**（真实启动实例 + 真实发请求），编译通过/单测绿都**不算**
- `.ps1` 必须带 **UTF-8 BOM**（PS 5.1 无 BOM 时按 GBK 读，中文注释会让语法崩）

---

## 七、外部依赖与凭据位置（都不在仓库内）

| 用途 | 路径 |
|---|---|
| updater 签名私钥 | `D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.key` |
| 私钥口令 | `D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.password` |
| 所有者网关配置 | `%USERPROFILE%\.wb-switch\gateway\gateway_native_config.json` |
| 账号凭证 | `%USERPROFILE%\.wb-switch\gateway_auths\` / `qoder\auths\` / `zcode\auths\` |

构建签名包：
```powershell
& ".\scripts\build-signed.ps1" -KeyFile "D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.key" `
                              -PasswordFile "D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.password"
```

---

## 八、有价值的复用资产

| 资产 | 位置 | 用途 |
|---|---|---|
| 可编排 mock 上游 | `crates\ai-gateway-router\src\testing\`（task-32） | 线上故障形态固化 |
| 不触网 runner 测试范例 | `crates\ai-gateway-router\src\products\product_tasks.rs` | 早退路径测试 |
| 假上游范例 | `crates\ai-gateway-router\src\upstream\tasks.rs` 测试模块 | `std::net::TcpListener` |
| 侦察报告 | `test-bin\recon\REPORT-{qoder,zcode,trae}.md` | 外部仓库功能清单 |
| 解包比对脚本思路 | `DELIVERY-1.0.14.md` 第四节 | 验证安装包内容而**不安装** |
