## AI Gateway 1.0.13 · 正式版

这是一个**合并与修复版**。它把上游 `momo0410/ai-gateway` 的能力按需并入本仓库，并修掉三处此前静默失效的缺陷。

> **如果你装的是 1.0.12 或更早**：请升级到 1.0.13。1.0.12 及更早版本**内嵌的 Go 网关是旧的**（详见第六节），本批所有网关侧改动都不在你正在运行的二进制里。

---

## 一、本次并入的内容

上游 PR#2（`momo0410/ai-gateway` → head `0697b89`，即 tag `v1.1.0`）与我们是**两条相反的路线** —— 它删掉 Go 网关、不接 Qoder、ZCode 只作为「可导入的客户端」。所以**没有整体合并，只做定点摘取**：取其能独立成立的部分，弃其架构改动。

| 批次 | 内容 | 状态 |
|---|---|---|
| 批次 0 | 修 `add8b527` 引入的**真实数据丢失**（详见第二节） | ✅ |
| 批次 1 | 核心模块合并：16 个新文件 + 10 个纯快进覆盖 + 4 处手工合并 | ✅ |
| 批次 1d | 前端：`api.ts` 22 类型 + 21 函数、19 条路由、20 个组件、演示模式配对 | ✅ |
| 批次 2 | 成长任务：补齐 `Sequential_Tasks_1` 等真实可执行任务 | ✅ |
| 批次 3 | ZCode 风控码提示、Trae 解密与双区域、Qoder 计费周期、Trae 静默失效修复 | ✅ |
| 批次 4 | 访客入口（按所有者决定跳过，理由见第七节） | 决策完成 |

### 保留未并的部分（有意）

- **18 个成长任务码**、8,448 行 Qoder、15,803 行 ZCode、三个协议入口、首帧预读 —— 上游 PR#2 把 Go 网关整个删掉换 Rust 重写，1.0.13 当时**不跟**。
  > ★ **2026-10-01 更新（1.0.14）**：所有者随后决定「我们也换成 rust 全重写」，故 1.0.14 已把网关**全量重写为 Rust**（走**多产品等价**路线，而非上游那条「删掉 Go 网关、不接 Qoder」的路线）。上面列的能力在 1.0.14 里全部保留。
- `Sequential_Tasks_4`–`_7`：上游只预留了槽位没有实现，我们同样只预留。
- 访客账号提交入口：见第七节。

---

## 二、批次 0：一次真实的数据丢失

`add8b527` 是一个**已实测的数据丢失 bug**，本次修掉：

| 数据 | 修复前 | 修复后 |
|---|---|---|
| `usage.json` 输入 | 1,809,167,437 | 1,809,167,437 |
| `usage.json` 输出 | 292,798,415 | — |
| `records` | 5,619 → **669** | 全部保留 |
| `cacheRead` | 1,745,539,712 → **282,975,744** | 全部保留 |

`gateway_config.json` 同时丢失了 `allowed_model` 与 `manual_uids` 两个键。修好后**全部回归**。

---

## 三、批次 2：成长任务真正可执行

此前 `Sequential_Tasks_*` 是唯一的**真实能力缺口**。现在 `_1` 已端到端跑通：

- 在真实 CN 账号上跑完，耗时 **58.2s**，返回 `"已上报 1 条小程序对话事件；已自动领奖 +100 积分 +5 能量"`，`progress 0/1 → 1/1`，`claimed: true`。
- 两条关键约束（踩过才知道）：
  - `Sequential_Tasks_1` **必须**带 `X-Client-Platform: miniprogram`，否则上游回 `{"status":"error","message":"task not found"}`
  - `school_season` **必须**带 activityId，而 `Sequential_Tasks_1` **必须不能**带
- `_2` 是**日期锁**（`unlock_at: 2026-10-01T00:00:00+08:00`），未到解锁时间会 `status:"skipped"` 并在 1.2s 内零事件返回 —— 这是正确行为，不是失败。
- 不可执行的任务按你的要求**已删除**（筛掉 5 个国际版码）。

---

## 四、批次 3：三处**静默失效**（本批最有价值的部分）

这三处都不是「报错」，而是「**看起来在工作、实际什么都没做**」。它们此前都有测试覆盖，但测试用的是**按错误实现构造的夹具**。

### 4.1 Trae 账号发现恒取不到数据

生产代码读的是**扁平键** `storage.get("icube_gtm.users")`，而真实 `storage.json` 里是**嵌套结构** `{"icube_gtm": {"users": {...}}}`。两者永不相等 ⇒ 该路径**恒返回空**。

修复后在**本机真实 `storage.json`**（只读）上实测：

```
修复前：Trae -> evidence_count=1（只有 state.vscdb 键名证据）
修复后：Trae -> read_storage_json = Some(top=26)
          [flat key] "icube_gtm.users" object=false entries=0   ← 旧写法恒为空
          [nested]   icube_gtm.users  object=true  entries=1
          discover_app -> 1 条; uid=2593***28 confident=true evidence_count=3
```

**反证**：把生产代码临时改回扁平键、夹具保持嵌套，`cargo test --lib trae_discover` 立刻 **4 failed**，其中两条是**本次修改之前就存在**的旧测试 ⇒ 直接证明**旧测试此前在替 bug 打掩护**。

### 4.2 Trae base64 判据与解码不共用同一条宽容路径

`looks_like_encrypted_base64`（判据）用严格解码器，而 `decode_base64_clean`（解码）是另一套 ⇒ 出现「判为密文但解不出来」的分叉，**静默跳过**。

修法是让**判据与解码共用同一个入口** `decode_base64_lenient`。实测不变式：44 条输入中判真 18 / 判假 26，**0 处分叉**。

**刻意不复刻** Node 参考实现的 Latin-1 截断（Node 把中文 `'是'` 的低字节当成合法 base64 字符）—— 复刻它会让**普通中文被误判为密文**，是纯误伤面。我们改判为 `None`，并有专门断言组钉住这个分歧。

### 4.3 Qoder 计费周期刷新完全没接线

`SetBillingCycle` / `CycleOf` 此前是**死代码**。现接入 15 分钟积分巡检循环（不是 `product_tasks`，避免与 nil 判据陷阱冲突）。

真实实例实测（隔离实例，只读凭证副本）：

```
/usage 顶层键 11 个（旧版 10 个），新增 billingCycles：
  <uid> -> { cycleStartAtMs: 1789371193945, nextResetAtMs: 1791963193945,
             known: true, records: 0, ... }
```

★ **落盘值与读取投影不同是正确设计，不是 bug**：
- 落盘 `usage.json` 写**原始边界** `1784014393945`
- `/usage` 读路径**投影**出 `1791963193945`（catch-up 一次：`+30 天`）

`projectCycleLocked` 刻意**不写回**（否则管理端轮询一次就会丢掉上一个周期的积分），而写路径才推进。两个账号的对照证明这一点：第二账号无需 catch-up ⇒ **写读一致**。

---

## 五、实测：同一台机器上的 A/B 对照

### 5.1 方法

用户已装 GUI 正在运行（单实例插件使并行启动新 GUI 不可行），所以改用**最强的可行路径**：把两个安装包里解出的内嵌网关二进制**各自真实跑起来**，除端口外配置**逐字节相同**。

- A = 旧 1.0.12 包内嵌网关（10,352,128 B）
- B = 新 1.0.13 包内嵌网关（10,438,656 B）
- 两侧共用同一批只读凭证副本（19 个 WorkBuddy + 2 个 Qoder + 2 个 ZCode），配置只差 2 行（`listen` 与 `state_file`）

### 5.2 结果

| 路由 | A（旧） | B（新） |
|---|---|---|
| `GET /healthz` | 200，51 B | 同 |
| `GET /status` | 200，8748 B | 同（**8748 B，逐字节相同**） |
| `GET /v1/models/regions` | 200，6276 B | 同（**6276 B**） |
| `GET /usage?days=1` | 200，272 B，**无** `billingCycles` | 200，515 B，**有** `billingCycles` |
| 未知路由 | 404 `text/plain` | 同 |
| `/usage` 无 Bearer | 401 | 同 |

**唯一结构差异就是计费周期** ⇒ 其余行为**逐字相同，无回归**。

★ 负对照有效：Go mux **没有** SPA fallback，未知路由返回 404 而非 200；`/usage` 无鉴权返回 401。**判定不能只看 HTTP 200**。

### 5.3 包内嵌的确实是新网关（逐字节）

```
dist\AI Gateway_1.0.13_x64-setup.exe
  SIZE   = 13,336,895
  SHA256 = 6f3a2b2e79b7917ad88ca64a61575580f549372036faef423f0ed4dcda9dde01

包内解出的内嵌网关：
  10,438,656 B  SHA256 80083ac910883908ff4a30e0be83cca9ac82d2c0c83b86ba265d0c0ca18a762b
磁盘 crates/ai-gateway-core/embedded/gateway.exe：
  10,438,656 B  SHA256 80083ac910883908ff4a30e0be83cca9ac82d2c0c83b86ba265d0c0ca18a762b
  ⇒ ★★★ 逐字节一致
```

**负对照**：同样方法验旧 1.0.12 包 → **FAIL**（解出 10,352,128 B / `000f779d…`，≠ 磁盘）。

前端同理：新包内嵌入口含 `assets/index-CciqdROI.js`，与 `dist/index.html` 引用一致；旧包是另外四个名字。

---

## 六、修掉一个**部署级**缺陷：内嵌网关一直是旧的

你此前反馈过「每次都会内嵌的是旧的网关」。本次**逐字节证实了它**：

```
旧 1.0.12 包内解出的网关：
  10,352,128 B  SHA256 000f779d93443c0ab6eaea9b77eba40bbb0ac2bbdd52292b95c6e0120588b51c

你机器上正在执行的文件：
  %USERPROFILE%\.wb-switch\gateway\bin\gateway-3eaaab-acebcb425ff218ae.exe
  10,352,128 B  SHA256 000f779d93443c0ab6eaea9b77eba40bbb0ac2bbdd52292b95c6e0120588b51c
  ⇒ ★ IDENTICAL
```

⇒ 你正在跑的网关**就是**旧包里那份 ⇒ **本批所有网关侧改动（含新路由与新命令）此前都不在你的二进制里**。升级到 1.0.13 后：

- 落地文件名从 `gateway-3eaaab-acebcb425ff218ae.exe` 变为 **`gateway-3f4554-f4646e258096b2c1.exe`**
- `gateway_embed::cleanup_old()` 会自动删掉所有名字不等于当前指纹的 `gateway-*.exe` ⇒ **旧的会被自动清理**

### 验证方法（本次新建，可复跑）

`build-signed.ps1` 的「校验 A」只验到磁盘产物，管不到「**打出来的 .exe 里到底装的是哪一份网关**」。新增 `test-bin/pkgverify/extract_pkg.py` 补上这一段：**全程不运行安装器**（只用 7-Zip 解包），从主程序里扫描 gzip 流、解压、与磁盘 `embedded/gateway.exe` 做 SHA256 比对。

---

## 七、按你的决定**未做**的两件事（如实说明）

### 7.1 访客账号提交入口 —— 跳过

不是「没来得及」，是三条**产品与合规**上的理由：

1. **与单机产品形态冲突** —— ZCode 模块的设计文档明确写着「import 是主路径，OAuth 是兜底」（`zcode_login.rs:5-13`）。对外开放提交入口会把产品从「本机工具」变成「服务端」。
2. **合规风险** —— 公开收集他人账号会**成为别人账号的囤积点**。
3. **暴露面** —— 网关监听在**所有网卡**上（`local=::`），不是仅本机。

### 7.2 求解器代理注入 —— 已知、未修

ZCode 人机验证的求解器子进程**不走**你配置的代理（`captcha.go` 里没有 `cmd.Env` / `os.Environ` / `HTTP_PROXY`）。**刻意不修**：

- 线上 `proxy_scope = {"cn":false,"intl":true}` ⇒ **国服流量本就不走代理**
- 而阿里云验证码是**国内**服务 ⇒ 给它注入代理可能**弄坏当前可用的路径**
- 没有真实 A/B 证据之前不改。

---

## 八、测试与验收

| 项目 | 结果 |
|---|---|
| `go build ./...` | EXIT 0 |
| `go vet ./...` | EXIT 0 |
| `go test -count=1 ./...` | **15 个包全 ok**（无 `(cached)`） |
| `npx tsc --noEmit` | EXIT 0 |
| `cargo test -p ai-gateway-core` | **1141 passed / 0 failed / 4 ignored** |
| `cargo test --test trae_crypt_crossimpl` | 9 passed / 0 failed |
| 真实实例 A/B | 见第五节 |
| 包内网关逐字节 | ✅ 一致 |

---

## 九、已知问题与降级项（如实列出）

1. **仓库 EOL / gofmt 不统一** —— `gofmt -l .` 标记 174 个文件（71 个纯 CRLF 噪声 + 103 个真未格式化），`core.autocrlf=true`。**刻意不做 `gofmt -w`**：会产生 1049 行假 diff 淹没真实改动。
2. **`CycleOf` 仍只被测试调用** —— 写路径 `SetBillingCycle` 已接入生产；读路径生产走 `cycleReportLocked` / `projectCycleLocked`，导出的 `CycleOf` 目前**仍只被测试调用**。
3. **无 `-race`** —— 本机**没有 gcc**，`go test -race` 报 `-race requires cgo`，无法运行。
4. **`credit_refresh_enabled=false` 时周期刷新一起停** —— 两者挂在同一个巡检循环上（有意如此，但需知悉）。
5. **Trae 双区域（US）仍是展示级** —— `trae_realm.rs` 的 `chat_host` / `auth_host` / `ide_version` / `oauth_client_id` / `infer_realm_for_model` **0 个生产调用方**；9 个 Trae 模块仍全部硬编码 CN host。SG 路径**从未对真实上游验证过**（手上只有 CN 账号）。
6. **`TRAE_SALT_A..D` env 覆盖未实现** —— 能力缺口，非正确性缺陷。
7. **`trae_oauth.rs:33` 的 SG client_id 保留未改** —— 该值在 CN auth host 上**已实测可用**，而 CN 的 client_id **未经验证** ⇒ 不拿已知可用的登录路径去赌未验证的路径。
8. **9074（上游繁忙）结果不被自动重试** —— 结果是 `"skip"`，而内联重试只重试 `status == "fail"`。9074 是**上游容量信号，不是账号惩罚**，所以不冷却账号；但这类 skip 目前不会被重试，需单独任务。
9. **`looks_like_tc_base64` 已无生产调用方** —— 按任务书**没有删**，只在其文档注明「保留为只认 tc 头这条语义的对照锚点」。
10. **`DecryptFailed` ↔ `HashMismatch` 边界不修** —— 约 0.4% 数据相关概率，属已知精度问题。
11. **测试代码有 4 条无害警告** —— 发布构建**只有**构建脚本那条信息性输出，无代码警告；4 条全在 `#[cfg(test)]` 里：`multi_product_credit_patrol.rs:259` 未使用的 `use super::*`、`product_credit_snapshot.rs:227` 未使用的测试辅助函数 `unique_uid`，以及两条**中文测试函数名**触发的 `non_snake_case`（`trae_realm.rs:495`、`trae_oauth.rs:555`）—— 中文测试名是**本仓库既有惯例**（`trae_checkin.rs` 等大量沿用），故保留不改。

---

## 安装

- **Windows**：`AI Gateway_1.0.13_x64-setup.exe`（NSIS，12.72 MB）
- 升级后**旧的 `gateway-3eaaab-*.exe` 会被自动清理**，无需手工处理

## 已知问题（用户可见）

- 人机验证**不会被绕过**（刻意如此）：遇到 `captcha_required` 请按提示在 ZCode 官方客户端里完成一次验证后重试
- 11140 的根因是**该账号在本设备上尚未完成登录**，网关只能冷却它并自动切到别的号；要让这个号恢复，需要在「账号管理」页**重新登录该账号**
