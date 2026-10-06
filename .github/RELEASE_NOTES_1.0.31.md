## AI Gateway 1.0.31 正式版

> 本版三件事：**① 合入上游参考仓库里可用的 6 项更新**（最要紧的是账号字段类型归一 —— 上游把整个 JSON 结构体直接透传给界面，React 渲染到对象会整页崩；以及静态模型表与网关侧取并集，此前两边各写各的，`/v1/models` 和界面下拉会对不上）；**② 排查你报的 `deepseek-v4.1-flash` 截断** —— 已定位到报障文案是**网关自己合成的那一帧**（措辞是 “**a** completion event”，而错误分支是 “**the** completion event”），并补上了那条分支此前**一条都没有**的日志，下次再遇到能直接从日志读出真凶；**③ 额外产出一个「非客户端纯部署」单文件产物**（`dist-single\ai-gateway.exe`，27 MB，无 GUI、无 WebView2、无安装器，拷到服务器直接 `serve`）。

---

## 一、合入上游的 6 项

| 改动 | 结果 |
| --- | --- |
| 账号字段类型归一 | **已修复**：新增 `display_string()` / `current_account_view()`，`id`/`uid`/`email`/`nickname`/`enterpriseName`/`needsReloginReason`/`note`/`domain`/`phoneNumber`/`accountType` 一律归一成字符串或 `null`。此前上游把 JSON 结构体直接透传给界面，React 渲染到对象会整页崩。时间戳字段保持原始数字。 |
| 状态出口统一 | **已修复**：宿主 IPC、webui API、CLI `status`、导入预览四处出口全部改走同一个函数，不再各写各的。 |
| 静态模型表并集 | **已修复**：宿主侧表与网关侧静态表取并集 = **30 条**（国服前 16 条保持原顺序 + 国际版去重后 14 条）。两侧各加**快照测试**锁死，防止再次漂移。 |
| 剩余天数向上取整 | **已修复**：原来截断会把 0.9 天显示成「0 天」，看起来像已过期。 |
| 轻量模式残留 | **已修复**：启动时显式清掉轻量模式标志。（上游对应 bug 是「轻量模式被持久化，重启后主窗口永不显示」；本仓库该标志是进程内的，结构上不可能复现，但显式清零更稳。） |
| 上下文超长码别名 | **已修复**：补上 `4028` 这个业务码，此前只有 `11115`。 |

> **另一条出处更正**（避免后来者以为是我们独创）：本版新加的**网关日志查看入口** —— `GET /api/gateway/log` + 网关页诊断区日志面板 + 「打开所在文件夹」—— 上游 `1c8bfed`（PR #31）**早就有同款**（`read_gateway_log` / `reveal_gateway_log` 两个 Tauri 命令、`logFile` / `logBytes` 两个状态字段，连命令名都一样）。我们是对齐它的做法，唯一差异是**取日志的口径**：上游按**行数**（`?maxLines=500`，上限 5000），我们按**字节**（`?bytes=N`，默认 64 KB，clamp 1 KB–1 MB）—— 网关日志里会夹带上游返回的超长单行 JSON，按行切容易切出半行。

**没有合入的上游提交（附理由，避免后来者重复评估）**：

- 子进程超时收尾 —— 本仓库实现更完整（有分平台的强制结束与 5 秒收尾等待）。
- 内容安全失败分类 —— **绝不能抄**：本仓库把 403 码 `11140` 判为**账号侧软冷却**，有 2026-09-28 的实测证据；抄过来会把正常软冷却误判成内容安全失败。

---

## 二、你报的 `deepseek-v4.1-flash` 截断

你的原文：`失败原因：upstream stream ended before a completion event; this turn may be incomplete`

**结论：这条是网关合成的截断帧**，即上游在吐过内容之后没按约定发 `[DONE]` 就结束了（不是客户端网络中断）。判据是措辞 —— 全仓库只有两处会产出这句话：

| 谁产的 | 文案 |
| --- | --- |
| **网关合成**（上游没发 `[DONE]`） | `upstream stream ended before **a** completion event; this turn may be incomplete` |
| 错误分支 / 上游解析 | `upstream stream ended before **the** completion event: …` |

你报的是 **a** ⇒ 第一种。

**为什么之前日志里查不到**：给客户端补这帧 error 的那条分支**一条日志都没有**；而另一处记账函数又主动排除含这段文字的帧 ⇒ 我们自己合成的帧永远进不了日志。所以不是「没发生」，是「根本没记」。**本版补上了**：

- 补帧分支现在会写：`chat uid=… product=… model=…: 上游未发 [DONE] 就结束，已向客户端补发 error 帧 code=upstream_stream_truncated（已收 N 个有效帧 / M 字节，saw_done=false）`；
- 上游读取出错分支同样补上帧数与字节数。

**已用假上游把六种断流形态各跑一遍**（`test-bin\mock-chat\`，可随时重跑）：正常收尾 / **HTTP 200 优雅收尾但不发 `[DONE]`** / socket 硬断 / 流中发错误帧 / 静默挂住 / 空流 —— **6/6 全部按设计收敛**。其中「优雅收尾不发 `[DONE]`」这一档，网关返回的字节**与你报障原文逐字一致**：

```
data: {"error":{"code":"upstream_stream_truncated","message":"upstream stream ended before a completion event; this turn may be incomplete","type":"upstream_error"}}
data: [DONE]
```

**下次再遇到**：打开网关页诊断区的日志面板（本版新增，做法对齐上游 `1c8bfed`），或直接看 `%USERPROFILE%\.wb-switch\gateway\gateway_data\gateway.log`，搜 `上游未发`。那一行会直接给出是哪个账号、哪个产品、哪个模型、收到多少帧多少字节。

---

## 三、本版新增：非客户端纯部署产物

```
dist\ai-gateway_1.0.31_server-x64.exe   27,274,752 B
SHA256 9FAA831BC3362C90976A8957B0AE67557A4E3A242D6FB8C220A44336D138669C
  （构建产物原名 wt-port\dist-single\ai-gateway.exe，与 target\release\ai-gateway.exe 逐字节一致）

启动：ai-gateway_1.0.31_server-x64.exe serve --port 57890
账号管理: http://127.0.0.1:57890/
账号管理: http://127.0.0.1:57890/
```

单文件、**无 GUI、无 WebView2 依赖、无 NSIS 安装器**，内嵌网关已打进同一个 exe ⇒ 只需分发这一个文件。已在隔离实例（独立数据目录 + `:57911`/`:57912`）实测：`version` 输出 `1.0.31`、`/api/status` 正常、网关起得来、端到端对话 `HTTP 200`、`/v1/models` 148 条。

顺带修掉一个真 bug：`scripts\build-single.ps1` 此前**跑不完** —— 到「编译 Rust 主体」就没声了，产物不生成、日志里连一句失败都没有。根因是 PowerShell 5.1 在 `ErrorActionPreference = 'Stop'` 下会把 cargo 写到 stderr 的**普通进度信息**当成致命错误并当场中止脚本。已改为只按退出码判成败。

---

## 四、客户端安装包

```
dist\AI Gateway_1.0.31_x64-setup.exe   14,586,903 B
SHA256 1FB28395FE386E160482BC6BEC1A2417809755C24B3521C125F56ABBEAADA2A5
  .sig  424 B  keyid 7e7ce64b6fc3a36f
```

你实机是 **1.0.21**，直接装 1.0.31 即可（中间所有版本的修复都在里面）。

★ 提醒：NSIS **同版本不覆盖**，所以每次交付都必须升版本号 —— 本版已升到 1.0.31。

---

## 五、验证与测试基线

| 项 | 结果 |
| --- | --- |
| `cargo test -p ai-gateway-core --lib` | 1285 passed / 0 failed / 4 ignored |
| `cargo test -p ai-gateway-router --lib` | 1669 passed / 0 failed / 1 ignored |
| 安装包独立解包（7-Zip，**不运行安装器**） | `Everything is Ok`，15 个文件，`FileVersion=1.0.31` |
| 包内是否含新代码 | 新符号 `read_gateway_log` / `reveal_gateway_log` / `logBytes` 均命中 |
| 隔离实例真机 | 端到端 `HTTP 200`、SSE 完整；`/v1/models` 148 条；账号字段确认为字符串 |
| 假上游断流矩阵 | 6/6 按设计收敛 |

全程未触碰你机器上正在运行的服务（`:7864` / `:3080` / `:57890` 实测期间始终在监听）。

---

## 六、本轮**未验证**的面（如实说明）

- 截断复现是在**本机假上游**上做的，能证明「上游不发 `[DONE]` ⇒ 网关补截断帧 + 留痕」这条链路，**不能**证明你那次的腾讯侧原因（上游断流 / 本地网络 / 代理都有可能）。新日志会给出答案。
- 「优雅收尾不发 `[DONE]`」和「socket 硬断」在真机上**没能复现**（随机遇到，无法按需触发）。
- 安装包**未在你的实机上安装**（你不在电脑旁，且约束禁止在你实例存活时运行安装器）。

---

## 七、仓库形态调整（本版顺带落地）

开发主线分支从 `migrate/aigw-v1.0.2` 改名为 **`main`**。那个名字来自「从 Go 网关迁到 Rust 网关」的时期，迁移早已完成，名字却一直留着，看仓库的人（包括以后的自己）会误以为还在迁移中。

- 原来的 `main`（`224919a`，`origin/main` 那条**陈旧且独立**的血统，与主线在 v0.7.0 那代就分开了）改名为 `archive/origin-main` 留档，不再参与开发；
- 改名是**纯本地操作**，没有 push、没有动 remote、没有建新仓库；
- 顺带补了 10 个 annotated tag（`v1.0.21` / `v1.0.23` – `v1.0.31`）—— 此前 **v1.0.11 起全部没有 tag**，「版本 ↔ 提交」只存在于提交信息和文档里，事后想 checkout 某个版本无从下手。现在 `git describe` 能正常给出 `v1.0.31`。
- ★ `v1.0.11` – `v1.0.20`、`v1.0.22` **补不了 tag**：这些版本号在任何提交的 `package.json` / `tauri.conf.json` 里都从未出现过 —— 包做出来了、`dist\` 里有 exe、文档写了，但当时没改版本号文件。查这些版本只能靠 `dist\AI Gateway_<版本>_x64-setup.exe`。

> 另记一条**已知现状**（不是本版引入的）：桌面端自动更新通道 `iuuuuuuuu/ai-gateway` 上的 `latest.json` 停在 **1.0.10**（2026-09-28），1.0.11 起从未发过 GitHub Release ⇒ 装 1.0.11 及以上版本的用户**不会**收到自动更新提示，需要手动下载安装。

