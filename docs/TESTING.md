# 测试与验收（TESTING）

> 门禁与基线、四层测试、假上游矩阵、真机隔离实例验收、**禁止假绿门禁**。
> 最后核对：2026-10-15 ｜ 分支 `main` ｜ 版本 **1.0.46** ｜ 核对时 HEAD `dc729e9`
>
> **什么时候翻它**：跑门禁、判断「这个绿是不是真的」、写新测试/新探针、怀疑基线数字过时。
> **什么时候别翻它**：想查「现在到哪了 / 有哪些未验证面」→ [../PROJECT-STATE.md](../PROJECT-STATE.md)；
> 想知道「当初为什么这么选」→ [DECISIONS.md](DECISIONS.md)；发版与打包步骤 → [RUNBOOK.md](RUNBOOK.md)。

---

## 0. 主门禁与当前基线

### 0.1 主门禁

```powershell
# cargo 不在新 shell 的 PATH 里 —— 先补工具链（见 §0.4）
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

Set-Location <仓库根>
npm run build                                          # tsc + vite，前端类型门禁
cargo test --workspace                                 # ★ 主门禁：全量单测
cargo check --workspace --all-targets --profile test    # 编译门禁（与 CI 同款）
```

**`--profile test` 不是可选项**：`cargo test` 默认在 `test` profile 下构建（继承 `dev`，
但 `debug-assertions` / `overflow-checks` 与 `dev` 不同 ⇒ 产物**不通用**），
而 `cargo check` 默认用 `dev`。不加这一条，同一个 job 里 `ai-gateway-core` 会被**完整编译两遍**
（Windows 实测第二遍 19–21s，而 `windows-latest` 正是 CI 的关键路径）。
`--all-targets` 会把测试代码也编一遍，所以**平台门控写错（少一个 `cfg`）在这里就会红**。

### 0.2 基线表（改动后**不得低于**这些数）

⚠ **表头版本 = 该数字被记录时的版本，不是「当前版本必然等于它」。** 全仓库文档里这套基线有
两套口径并存，**以版本更晚的记录为准**：

| 命令 | 最新已知 | 口径来源 |
|---|---|---|
| `cargo test --workspace`（15 个 test 块累加） | **3081 passed / 0 failed / 7 ignored** | 1.0.36（`PROJECT-STATE.md` §9.11「1.0.36 的构建与真机实测」里的「测试基线」，2026-10-08/10-09） |
| `cargo test --workspace` → `ai-gateway-router` 分项 | **1780 passed / 0 failed / 1 ignored** | 1.0.46（2026-10-14，`PROJECT-STATE.md` §9.21；1.0.45 为 1776，净增 4） |
| `cargo test --workspace` → `ai-gateway-core` 分项 | **1301 passed / 0 failed / 4 ignored** | 1.0.46（2026-10-14，`PROJECT-STATE.md` §9.21） |
| `cargo test -p ai-gateway-router --lib` | **1685 passed / 0 failed / 1 ignored** | **1.0.34 旧口径，已被 1780 取代**；证据 `uitest\ut-1034-sse.log` |
| `cargo test -p ai-gateway-core --lib` | **1285 passed / 0 failed / 4 ignored** | **1.0.34 旧口径**（与 1.0.31/1.0.32/1.0.33 同值），已被 1301 取代 |
| `cargo test -p ai-gateway-server` | **28 passed / 0 failed** | **1.0.20 口径**（`PROJECT-STATE.md` §三；最初记录见 `.github/RELEASE_NOTES_1.0.16.md`） |
| `cargo check -p ai-gateway-router --lib` / `--bin gateway` | EXIT 0，**0 warnings** | 1.0.34（`PROJECT-STATE.md` §三） |
| `cargo check --workspace --all-targets --profile test` | EXIT 0 | 与 CI 同款（`.github/workflows/ci.yml`） |

★ **1.0.46 的 `cargo test --workspace` 总计数没有单独记录**（§9.21 只记了 core / router 两个分项）
⇒ **未证，别编**。要更新基线数字，只能自己跑一次并落日志。

★ **「15 个 test 块」是可复核的**：我（2026-10-15）直接解析 `uitest\ws-test-1034.log`，
数出 12 个 `Running …` 头 + 3 个 `Doc-tests …` 头 = 15，累加 `test result:` 行得
**3045 passed / 0 failed / 6 ignored**。15 块分别是：

```text
ai-gateway_lib(lib) / ai-gateway(main) / ai-gateway_core(lib) /
ai-gateway-core tests\trae_crypt_crossimpl.rs / ai-gateway_router(lib) /
gateway(router bin) / probe_state(router bin) / wb2g-ref(router bin) /
ai-gateway-router tests\ab_classify.rs / ab_devicetoken.rs / ab_prompt.rs /
ai-gateway-server(ai-gateway, main) / Doc-tests ×3
```

⇒ **新增一个集成测试文件（`crates/*/tests/*.rs`）会改变「块」的个数**，基线表里的「15」也是被门禁的东西。

★ **`-p ai-gateway-router` 与 `-p ai-gateway-router --lib` 不相等**：裸 `-p` 会额外带上
`src/bin/{gateway,probe_state,wb2g-ref}.rs` 三个 bin 目标与 doc-tests。
`1685` 这个数是 **`--lib`** 口径（`.github/RELEASE_NOTES_1.0.34.md` §4.1 与 `ci.yml` 都写 `--lib`）。
**报数时把完整命令行一起写下来**，否则下一个人复现不出同一个数。

### 0.3 基线数字的交叉核对（2026-10-15，我逐个解析了日志）

`uitest\*.log` 是一组可以自证的基线证据。我逐个解析 `test result:` 行，得到：

| 日志 | 累加 passed / failed / ignored | 对应版本 |
|---|---|---|
| `ws-test-1031.log` | 3029 / 0 / 6 | 1.0.31 |
| `ws-test-1032.log` | 3038 / 0 / 6 | 1.0.32 |
| `ws-test-1033.log` | 3041 / 0 / 6 | 1.0.33 |
| `ws-test-1034.log` | **3045 / 0 / 6** | **1.0.34** |
| `ut-1034-sse.log` | 1685 / 0 / 1（router） | 1.0.34 |
| `ut-1032-trae-auth.log` | 1681 / 0 / 1（router） | 1.0.33 |
| `ut-1031-core-final.log` | 1285 / 0 / 4（core） | 1.0.31 |
| `ut-1031-router-final.log` | 1669 / 0 / 1（router） | 1.0.31 |

这套读数与 `PROJECT-STATE.md` §三 的版本序列（1.0.31 → 1.0.34 的 3029/3038/3041/3045）
**逐条吻合** ⇒ §三 表里的 1.0.34 口径可信。**但它已经过期**（仓库在 1.0.46），见 §0.2。

⚠ **这些日志是 UTF-16LE**（PowerShell `*>` 重定向的默认结果）。
用 `Get-Content` 读会正常，用 Node `readFileSync(p,'utf8')` 读**全是 NUL 夹杂** —— 解析前先判 BOM。

### 0.4 环境前提

```powershell
# cargo / rustc 不在新 shell 的 PATH 里；实测在 %USERPROFILE%\.cargo\bin\
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# 构建签名包还需要 rustup 工具链的 bin（本机为
# %USERPROFILE%\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin）
```

★ **`cargo` 缺失时报的错完全不提 PATH** —— 表现为
`failed to run 'cargo metadata' ... program not found`，极易误判成「Rust 环境坏了」。

★ ★★★ **绝对不许加 `--offline`**：这条规矩的原始理由是「本机 cargo 的 registry 缓存是空的，
加 `--offline` 会直接 `error: no matching package named 'aes' found`」（2026-10-05）。

> ⚠ **2026-10-15 复测：原始理由已不成立，但规矩保留。** 实测本机 registry 缓存**非空**
> （两个 source 目录共 1600+ 个 `.crate`，`aes-0.8.4.crate` 在）⇒ 现在加 `--offline` 未必立刻报错。
> **保留的三条理由**：① CI 与别人的机器**没有**这份缓存（`~/.cargo` 不入库），门禁必须与 CI 同形；
> ② 缓存是历史积累，新依赖照样解析不出来；③ `--offline` 会把网络失败伪装成「包不存在」，**误诊**。
> 详见 [AGENTS.md](AGENTS.md) §2.3。
⚠ **2026-10-15 复测：缓存现在不是空的**（`%USERPROFILE%\.cargo\registry\cache\` 下两个 source 目录
各有内容，`aes-0.8.4.crate` 也在）。**但规矩保留** —— 本仓库的权威口径（根 `AGENTS.md`、
`docs/AGENTS.md` §2.3、`PROJECT-STATE.md` §三）都仍写着禁令，且 CI 与别人的机器上没有这份缓存。
镜像走 `wt-port\.cargo\config.toml` 的 `sparse+https://rsproxy.cn/index/`；
⚠ **该文件被 `.gitignore` 排除、不入库** ⇒ 新 clone 的机器拿不到镜像，会遇到
`SSL connect error (schannel: failed to receive handshake)`。

---

## 1. ★★ 为什么 `--workspace` 与 `-p` **不是同一个构建**

**这一节是理解「同一个测试为什么两种跑法结果不同」的关键。**

### 1.1 机制（2026-10-15 在依赖图与上游源码层核实）

链条（每一环都可复核）：

1. `Cargo.lock` 里 `reqwest` 有**两个版本**：`0.12.28`（我们自己的 core / router 用）与
   `0.13.4`（`src-tauri` 的 `tauri-plugin-updater 2.12.0` 用）。
2. `tauri-plugin-updater` 的 `default = ["rustls-tls", "system-proxy", "zip"]`
   ⇒ 它启用 `reqwest 0.13.4` 的 `system-proxy`。
3. `reqwest` 的 `system-proxy = ["hyper-util/client-proxy-system"]`（0.12.28 与 0.13.4 都这么写）。
4. 而 **`hyper-util` 全 workspace 只有 0.1.20 一个版本** ⇒
   `client-proxy-system` 这个 feature 落到**同一个** `hyper-util` 上。
5. `hyper-util` 的该 feature = `["dep:system-configuration", "dep:windows-registry"]`，
   而 `src/client/proxy/matcher.rs` 里：

   ```rust
   #[cfg(all(feature = "client-proxy-system", windows))]
   win::with_system(&mut builder);

   #[cfg(feature = "client-proxy-system")]
   #[cfg(windows)]
   mod win {
       pub(super) fn with_system(builder: &mut super::Builder) {
           let settings = windows_registry::CURRENT_USER
               .open("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings") …
           if settings.get_u32("ProxyEnable").unwrap_or(0) == 0 { return; }
           … builder.http / builder.https ← "ProxyServer"
           … builder.no        ← "ProxyOverride"
       }
   }
   ```

6. 我们自己的 `reqwest 0.12.28` 的 `proxy.rs` 调用 `hyper_util::client::proxy::matcher::Matcher::from_system()`
   —— 同一个 `hyper-util`，**feature 是共享的**。

⇒ **在 `--workspace` 构建里，我们自己的 reqwest 也拿到了「读系统代理」的能力；在 `-p` 构建里没有。**

### 1.2 后果（这才是危险的地方）

- `reqwest` 默认 `auto_sys_proxy = true`，**会读 Windows 注册表**的系统代理；
  而 Go 的 `http.ProxyFromEnvironment` **只读环境变量**。
- ⇒ 「没配代理」这件事，在 Go 是真直连，在 reqwest 是**偷偷走代理**。
  **同一个「不配代理」的配置，两种构建的行为不一样。**

### 1.3 规矩

- 未配置代理时**必须显式 `.no_proxy()`**（`crates/ai-gateway-router/src/upstream/client.rs`
  的 `build_proxy`：无代理分支调 `http_builder.no_proxy()` / `chat_builder.no_proxy()`）。
- ★ **凡涉及代理的断言，必须在 `--workspace` 下至少跑一遍** —— `-p` 跑绿不能替代它。
- 这条缺陷类的**真实先例**（1.0.14）：假上游的请求被本机 `:7890` 的 mihomo 重新发起，
  代理（Go 程序）补上自己的 `User-Agent: Go-http-client/1.1`，连不上时拿到 **502** 而不是传输层错误。
  A/B 证据（同一条 `cargo test --workspace --lib`）：

  | 状态 | 结果 |
  |---|---|
  | 缺陷在（`None => {}`） | **2 FAILED** —— 断言打印出 `User-Agent: Go-http-client/1.1` 与 `502` |
  | 修复后（`.no_proxy()`） | **1403 passed / 0 failed** |

  ★ 且那条回归测试**做过反向验证**：把修复还原后新测试确实变红并打印出被注入的 UA。
  **第一版只断言「请求到达」，走不走代理都能过 —— 那是假绿，已重写。**（见 §5）

⚠ **本节结论我没有在 2026-10-15 用构建复验**（本任务禁跑 cargo）。以上 6 环是在
`Cargo.lock` + `%USERPROFILE%\.cargo\registry\src\` 里的 `hyper-util-0.1.20` / `reqwest-{0.12.28,0.13.4}`
/ `tauri-plugin-updater-2.12.0` 的 `Cargo.toml` 与 `.rs` 上**逐条读过**的。
要断言「两种构建的 feature 集合确实不同」，`cargo tree -e features -p ai-gateway-router` 与
`cargo tree -e features --workspace | Select-String client-proxy-system` 各跑一次即可对照。

---

## 2. 四层测试

| 层 | 内容 | 落点 | 能证明什么 / 不能证明什么 |
|---|---|---|---|
| **L1 单元测试** | 纯函数与单进程逻辑；毫秒级；**不触网** | 各 `src/**` 的 `#[cfg(test)] mod tests` | 能证明「判定逻辑对不对」；**证明不了**打包后能跑、证明不了真链路的字节形态 |
| **L2 假上游集成测试** | 真 socket / 真 HTTP，上游可编排 | `crates/ai-gateway-router/tests/*.rs`、`crates/ai-gateway-router/src/testing/mock_upstream.rs`、`crates/ai-gateway-core/tests/trae_crypt_crossimpl.rs` | 能证明协议适配、重试、熔断、收尾判据；**证明不了**真实上游的行为、证明不了 GUI |
| **L3 真机隔离实例验收** | 真实启动一个新构建的实例 + 真实发请求 | 本机 `test-bin\inst-*`（**不入库**）、探针脚本 | 能证明「这个产物真的能跑」；**证明不了**上游真值（假上游是死的） |
| **L4 包内产物自证** | 从**交付物本身**里解出二进制并比对指纹 | 解包比对 / 双指纹校验 / 验签 | 能证明「你测的就是你要交的那份」；**证明不了**功能正确 |

### 2.1 L1 的边界（有测试 ≠ 覆盖到）

- `crates/ai-gateway-router/src/testing/mod.rs` 是 `#[cfg(test)] mod testing;`
  —— **整个子树只在 `cargo test` 时参与编译，`cargo build` 的产物里一行都不会有**。
  ⚠ **不要**把它改成非 `cfg(test)` 的 `pub mod`：那是把测试夹具打进发行包。
- `bin/gateway.rs` 是**独立 crate**，看不到 lib 的 `pub(crate)` 项。
  需要 `pub(crate)` 依赖的逻辑必须在 lib 内做成 `pub` 入口，bin 只调用它
  （范例：`products/product_tasks.rs`、`products/qoder/billing_cycle.rs`）。
- **零散的假上游每处各写一遍**是 L2 的历史形态；有状态的、可按账号/调用次数编排的 mock
  集中在 `src/testing/mock_upstream.rs`（见 §3）。

### 2.2 L2 的一个真实教训：夹具指向了被 gitignore 的目录

`crates/ai-gateway-core/tests/trae_crypt_crossimpl.rs` 的夹具原先指向 `../../../test-bin/verify-trae/`，
而 `/test-bin/` 在 `.gitignore` 里（`.gitignore:109`）**从不进仓库**。后果：

- **CI 上 checkout 出来根本没有 `test-bin/` ⇒ 7 条测试红**（另 2 条不读夹具）；
- 而**开发机上永远绿** —— 因为那里有 `test-bin/`。

★ 规矩：**凡是被 `.gitignore` 排除、但测试或构建依赖的路径，都必须先问一句「CI 上它从哪来」**。
同型问题还有 `dist/`：`ai-gateway-server` 用 `RustEmbed` 嵌 `../../dist/`，
而 `dist/` 也在 `.gitignore` 里 ⇒ 刚 checkout 出来**根本不存在**，
`#[derive(RustEmbed)]` 直接报 `folder '…/../../dist/' does not exist`（并连带两条 E0599，
那两条报错指向的位置与病因完全不同，极易误读成 rust_embed 坏了）⇒ **CI 里必须先 `npm run build`**。

### 2.3 L4 包内产物自证（交付前必做，**不需要安装**）

**绝对不要运行安装包 / 卸载器**：Tauri 的 NSIS 模板按**可执行文件名**匹配并结束进程
（`src-tauri/installer.nsi` 的 `:651` 与 `:788` 两处 `!insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"`，宏体走
`nsis_tauri_utils::FindProcessCurrentUser` + `KillProcessCurrentUser`；**静默模式 `/S` 下不询问、直接杀**）。
主程序名与所有者正在运行的实例**同名**，而它是 Job Object（`KILL_ON_JOB_CLOSE`）的父进程
⇒ 一次误装会连带打掉 `:7864` 网关。

替代做法（三重闸，`scripts/build-signed.ps1` 的 `[3/4]` / `[4/4]` 步已固化）：

1. **内嵌网关新鲜度**：构建前后 `crates\ai-gateway-core\embedded\gateway.exe` 的 SHA256 必须一致；
   若设了 `AI_GATEWAY_ROUTER_BIN` 且它指向**另一份**网关，**直接失败**
   （`build.rs` 的查找顺序里环境变量**优先于** `embedded/`，那会让内嵌的不是你构建的那份）。
2. **内嵌前端新鲜度**：只能用 **`dist/index.html` 里写死的资源文件名**（`assets/index-XXXX.js`，
   Vite 按**内容哈希**生成、在 exe 里是**明文**）。
   ⚠ **在 exe 里搜业务字符串是错的判据** —— Tauri 对 JS 内容做了压缩/编码，
   连**早就存在**的老标识也搜不到。**搜不到 ≠ 没打包。**
3. **解包比对**：扫描 gzip 魔数 `1F 8B 08`，解压后应以 `4D 5A`（MZ）开头、> 1MB，
   再与 `crates\ai-gateway-core\embedded\gateway.exe.sha256` 比对。

```powershell
$want = (Get-Content crates\ai-gateway-core\embedded\gateway.exe.sha256 -Raw).Trim()
# 再扫描 gzip 魔数 1F 8B 08 → 解压 → 断言 MZ 头且 >1MB → 哈希比对 $want
```

★ 「编译通过 + 单元测试绿 + 解包比对 SHA256 一致」这三项**都证明不了功能可用**。
真实教训（2026-09-21，根 `AGENTS.md`）：当时把这三项当成了交付标准，
而实际交付的包里 WebView2 求解**从未成功过**，却因为**本地 Node 兜底**而表现为「能用」
—— 在本机看到 200 就以为成了。
**判定「功能是否真的生效」必须绕过兜底路径。**

---

## 3. 假上游矩阵

### 3.1 两种形态，别混用

| 形态 | 落点 | 并发模型 | 适用 |
|---|---|---|---|
| **A. 测试内嵌 mock**（推荐） | `crates/ai-gateway-router/src/testing/mock_upstream.rs` | `std::net::TcpListener` + **单后台线程** + **keep-alive 内层循环** | `cargo test` 内、随机端口、不触网、可断言收到的请求 |
| **B. 外部假上游进程** | 本机 `test-bin\inst-*\fake-upstream.mjs` / `*.js`（**不入库**） | Node `http.createServer` / Python `ThreadingHTTPServer` | 真机隔离实例验收（跨进程、可 A/B 两个网关打同一个假上游） |

### 3.2 ★★ 「单线程会死锁」这条要求的真实来源

`PROJECT-STATE.md` §四·6 写的是：**假上游用 `ThreadingHTTPServer` + `daemon_threads`；单线程会让网关死锁**
—— 那是**外部假上游进程（形态 B）**的约定。

形态 A 的 mock **故意是单线程的**（`MockUpstream::start()` 里一个 `std::thread::spawn` +
`for stream in listener.incoming()`），理由是 **Rust 测试里的用法都是串行请求**；
它靠**内层 keep-alive 循环**（一个连接承载多个请求）避免「复用连接的客户端在第二个请求上挂死」：

```rust
// 一个连接可能承载多个请求（keep-alive）—— 逐个处理，直到对端关闭。
// ⚠ 不处理 keep-alive 会让复用连接的客户端在第二个请求上挂死（reqwest 默认复用）。
loop { let Some(req) = read_request(&mut s) else { break }; … }
```

⚠ **`read_request` 里没有 `set_read_timeout`**（全仓库 `grep -rn set_read_timeout crates/` = 0 命中）
⇒ 若某个测试**并发**打这个 mock，第二个连接会在 `accept` 队列里排队，
而在 `incoming()` 上阻塞的线程抽不出身 ⇒ **看起来像网关卡死，实际是测试装置卡住**。
**要做并发/多账号风暴的用例（例如 `tests/ab_emptystream.rs`），用 `tokio::net::TcpListener` + `axum::serve`
（该文件就是这么写的），或者给 mock 加 per-connection 线程。**

> 这是一个**「测试装置自身伪装成产品缺陷」**的典型形状。同型问题还有两处，见
> `PROJECT-STATE.md` §四·8（CDP 注入落进 `about:blank` 上下文；期望值写死成日历常量）。

### 3.3 控制协议与观测协议（形态 A）

按**请求头**编排，因此**无共享状态、可并行跑测试、每个用例自带脚本**：

| 头 | 语义 |
|---|---|
| `x-mock-scenario` | 本请求的故障场景（缺省 `ok`） |
| `x-mock-sequence` | 按**调用次数**依序消费，耗尽后保持最后一个 |
| `x-mock-bind` | 与凭证绑定；**网关不会带这个头**，测试侧先注入到账号的上游头里即可实现「按账号注入」。缺省回落到 `authorization` / `x-api-key` 前 16 字符 |
| `x-mock-sse-truncate-at` | SSE 在第 N 个分片处截断 |

观测：响应头回 `x-mock-call-index`（该 bind 第几次被调用，从 0 开始）；
`MockUpstream::calls()` 记录全部 `(method, path, headers, body_bytes)`。

### 3.4 模式矩阵（★ 对照组和失败组一样重要）

**1.0.34 的 5 模式「断流形态」矩阵**（真实实现在
`test-bin\inst-1034-sse\fake-upstream.mjs`，网关侧由 probe 脚本按 `model` 名里的 `mode[A-E]` 选模式）：

| 模式 | 假上游行为 | 1.0.33 出厂网关 | **1.0.34** |
|---|---|---|---|
| A | 2 帧 + `finish_reason` → 150 ms 后 RST | 补 error 帧 ❌ | **无 error 帧** ✅ |
| B | 半句就 RST（无 `finish_reason`） | 报截断 | 报截断（**保持**）✅ |
| C | 吐完 + `finish_reason` + 干净 `end()`（无 `[DONE]`） | 补 error 帧 ❌ | **无 error 帧** ✅ |
| D | 只吐一帧就 `end()` | 报截断 | 报截断（**保持**）✅ |
| E | 完整（含 `[DONE]`） | 正常 | 正常 ✅ |

**每条流都要断言的量**：`doneCount`（`data: [DONE]` 出现次数）**恒为 1**
—— 不会出现双终止字节。B/D 两档是**反向对照**：证明修复**没有把真截断吞掉**。

**1.0.31 的 6 模式矩阵**（另一组，按收尾形态切）：正常收尾 / HTTP 200 优雅收尾但不发 `[DONE]` /
socket 硬断 / 流中发错误帧 / 静默挂住 / 空流 —— 6/6 按设计收敛。

**1.0.35 的 3 形态**（合并进真机装置）：正常 ⇒ 不补；只 `finish_reason` ⇒ 只补 `[DONE]` 不报错；
真截断 ⇒ 补 error 帧 + `[DONE]`。断言**响应体逐字节**：`[DONE]` 恒 1 个且 error 帧在它**之前**。

**内置 `Scenario` 枚举**（`mock_upstream.rs`，`Scenario::parse` 的字符串名即上表左列）：
`ok` / `rate_limited_quota_body` / `rate_limited` / `risk_control_3012` / `captcha_3007` /
`captcha_challenge` / `sse_truncate` / `empty_stream` / `auth_invalid` / `server_error` /
`quota_exhausted` / `not_found` / `garbage_body`。
★ 其中 **`rate_limited_quota_body`（429 但 body 是额度文案）** 与
**`risk_control_3012`（405 承载 `{"code":3012,…}`）** 是两条**线上实测**形态，
这个有状态 mock 存在的理由就是**把线上故障形态固化成回归测试**。

★ **三种协议都要各跑一遍**：`/v1/chat/completions`（OpenAI）、`/v1/messages`（Anthropic）、
`/v1/responses`（Responses）。1.0.34 的探针断言：模式 C 下 `/v1/messages` 200 + `event: message_stop`
且**无 error 事件**；`/v1/responses` 200 + `response.completed`；模式 B 下分别有 error 事件 / `response.failed`。

---

## 4. 真机隔离实例验收

### 4.1 三条铁律（违反会伤到所有者）

1. **绝不运行安装包 / 卸载器**（含 `/S`）。
2. **绝不按进程名批量结束进程** —— 只允许 **PID + 可执行文件路径双条件**：

   ```powershell
   $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$pid" -ErrorAction SilentlyContinue
   if ($proc -and $proc.ExecutablePath -like "*test-bin*") { Stop-Process -Id $pid -Force }
   ```

3. **绝不直接运行共享目录里的可执行文件 —— 哪怕只是 `--version`**。
   判据不是**你打算干什么**，而是**你有没有碰那个共享文件**。
   `crates\ai-gateway-core\embedded\gateway.exe` 被谁运行，Windows 就对它加锁 ⇒
   那之后的任何 `cargo build` 都会报 `failed to remove file … os error 5`；
   而且网关的 `main()` **根本没有 `--version` 分支** ⇒ 参数会被当成「无 config 启动」，
   **照常起 HTTP 服务并常驻**（默认 `:7863`）。
   要检查就先 `Copy-Item` 到自己的目录。

**每次验证前后都自检**（所有者 `:7864` 网关 / `:3080` DSH Desktop 必须保持在线）：

> ⚠ **2026-10-15 更正**：本节此前照抄根 `AGENTS.md` 写 `:43120`。实测（可复现）：
> `:7864` → PID 13884 = `gateway-509bdb-*.exe`；`:3080` → PID 38744 = `node.exe`（DSH GUI）；
> `:43120` → **0 条连接，未监听**。⇒ 拿 `43120` 当判据会**误报「业主服务掉了」**，
> 进而触发「立即停止并报告」的假警报。根 `AGENTS.md` 与 `PROJECT-STATE.md` §六 已同步为 `:3080`。
> **端口会变，判据是现场实测。**

```powershell
foreach ($port in @(7864, 3080)) {
    $c = Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue
    if (-not $c) { Write-Warning "所有者的 :$port 未在运行 —— 立即停止并报告" }
}
```

### 4.2 探针的硬约定

| 约定 | 内容 |
|---|---|
| **独立名** | 与所有者所有进程**不同名**（产物默认 `ai-gateway.exe`，必须改名） |
| **独立端口** | 避开 `7864` / `3080` / `57890`；惯用 `5789x` / `5793x` |
| **独立数据目录** | `$env:AI_GATEWAY_HOME` 指向 `test-bin\inst-*`，并验证目录里**只有日志与 exe** |
| **独立 target** | `cargo build --release -p ai-gateway-server --target-dir <独立目录>` —— `ai-gateway-server`（约 15MB）与 `src-tauri`（约 30MB）的 `[[bin]] name` **都叫 `ai-gateway`**，输出到同一个 `target\release\ai-gateway.exe` **互相覆盖**。**产物约 15MB = server 版；30MB 就是 GUI，别用**（GUI 带单实例插件，发现所有者的实例在跑就**自己静默退出**，无输出、不监听 —— 极易误判成「代码启动即退出」） |
| **GUI 侧要独立 identifier** | 用 `--config override.json`（`{"identifier":"com.test.<probe>","bundle":{"createUpdaterArtifacts":false}}`）⇒ 独立单实例锁 + 独立 WebView2 数据目录，与所有者实例并存 |
| **重定向才看得到日志** | GUI 的 stdout/stderr **不进 PowerShell 管道**；`Start-Process` 必须显式 `-RedirectStandardOutput` / `-RedirectStandardError` |
| **期望值现读，不许写死** | 版本号从 `crates\ai-gateway-router\Cargo.toml` 现读；日历相关的值从真机数据推（详见 §5.4） |
| **收尾按双条件清场并断言残留为 0** | `leftovers: gw=0 srv=0`，端口全释放 |

### 4.3 「实测」的定义（三者缺一不可）

1. **真实启动**一个**新构建**的实例（独立端口 + 独立数据目录）；
2. **真实发请求**走完整链路（`curl` 打到 `/v1/chat/completions`，**不是只跑单测**）；
3. 看到**期望的输出**（200 + 正确内容；或明确的、可解释的错误）。

**以下都「不算实测」，不得据此交付**：编译通过 / `cargo check` / `go build`；
单元测试全绿；产物 SHA256 与源码比对一致；
「我在自己的实例上看到 200」—— **除非那是当次构建的产物**。

⚠ **`curl` 的引号陷阱**：PowerShell 用单引号包 JSON 会**吃掉内部双引号** ⇒
请求体变成 `{model:xxx,...}`（非法 JSON），上游报 `11101 invalid character 'm'`。
**看起来像网关 bug，实际是测试方法错了。** 一律用文件传：

```powershell
$json = '{"model":"qoder:Qwen3.8-Flash","messages":[{"role":"user","content":"hi"}],"stream":false}'
[System.IO.File]::WriteAllText("$tmp\r.json", $json, (New-Object System.Text.UTF8Encoding($false)))
curl.exe -s -X POST "http://127.0.0.1:$port/v1/chat/completions" `
  -H "Authorization: Bearer sk-123" -H "Content-Type: application/json" `
  --data-binary "@$tmp\r.json" --max-time 60 -w "`n[HTTP %{http_code}]"
```

### 4.4 真机验收也有覆盖不到的面（诚实记账，别把真机当万能）

- **GUI 侧（托盘 / 单实例 / 自动更新 / 开机自启）在设计上做不了**：桌面版第一个注册的插件是
  `tauri-plugin-single-instance`（互斥量 `com.momo0410.aigateway-sim`）；
  只要所有者的实例在跑，解包出来的 GUI exe 只会 `exit(0)`。
- **假上游是死的 ⇒ 任何依赖上游真值的路径都验不到**。真实先例：`credit_floor` 闸
  因为积分巡检刷不回真值 ⇒ `credits_known` 恒 `false` ⇒ 「未知不拦」的设计让它在整轮
  **27 个 PASS 里一次都没被触发过**。1.0.46 才用一个**本机假积分上游**
  （Node，只监听 `127.0.0.1:57940`，按真实契约回双层信封）把它补上。
  ★ 规矩：**报 PASS 数时必须同时说「这轮哪些分支一次都没走到」**。
- **前端/UI 验证那条路当前不可执行**：根 `AGENTS.md` 引用的 `uitest/page-harness.cjs` /
  `uitest/mock-host-api.cjs` / `uitest/README-验证约定.md` **在磁盘上不存在**
  （2026-10-15 实测：`uitest\` 下只有 22 个 `.log`，全仓库查找无匹配；根 `AGENTS.md` 的更正
  另记载它们**从未入库**）。⇒ **要按那条路走，需先重建这些探针。**

---

## 5. ★★★ 禁止假绿门禁

**任何测试 / 校验 / CI 都不得「显示通过但实际没验证到东西」。**

### 5.1 形态清单（本项目实际见过的，逐条给先例）

| # | 形态 | 本仓库的真实先例 |
|---|---|---|
| 1 | **断言被跳过 / 吞异常** | 流式收尾的断言只覆盖了「请求到达」这一层，走不走代理都能过 ⇒ **假绿**，1.0.14 重写后才有意义 |
| 2 | **`#[test]` 属性挂空 ⇒ 测试从未被执行** | `gateway.rs` 里一个 `#[test]` 夹在两段文档注释**之间** ⇒ 属性归给了下面那条测试（`duplicated attribute`），而**上面那条**（`gateway_restart_goes_through_serialized_entry`，「不得绕过串行化入口」的护栏）**退化成普通私有函数**、报 `never used` ⇒ **它从未被执行过**（1.0.36 引入，1.0.37 CI 修） |
| 3 | **mock / 桩替掉真实逻辑** | 外部求解失败 → **回退本地 Node**。于是外部全坏时，只要本机装了 Node，对话照样成功 ⇒ 本机 200 **证明不了**宿主求解链路可用 |
| 4 | **脚本恒 `exit 0`（静默死亡）** | 1.0.31 修掉的 `build-single.ps1` 静默死亡 bug；同类先例还有 `node_modules/.bin` shim 产生**假的 exit 0**（末尾 echo 成功、构建其实没跑） |
| 5 | **只跑编译或单测就宣称通过** | 2026-09-21：「编译通过 + 单测绿 + 解包 SHA256 一致」被当成交付标准，而包里 WebView2 求解**从未成功过** |
| 6 | **失败用例被注释 / 降级** | 1.0.9 → 1.0.23 期间 `ci` 与 `release` **每一次都是红的**，而发版一直手工做 ⇒ **没人发现**。`release.yml` 的 preflight 挂在「核心单测」⇒ 四平台 build 与「发布 Release」两步**全被 skip**，等于 tag 打上去什么都不会发生（2026-10-09 业主追问后才查实） |
| 7 | **占位 always-pass / 用 `\|\| true` 蒙退出码** | 见 §5.3 的 `check-1034.log`：PowerShell 把 cargo 的 stderr 包成 `NativeCommandError`，**诱人直接把它抹掉** —— 而那一步就把真检查也一起消掉了 |
| 8 | **本地全绿 / CI 全红长期共存** | 被 `.gitignore` 排除但测试依赖的 `test-bin/`、`dist/`（见 §2.2） |
| 9 | **装置自身的偏差被当成产品缺陷** | CDP 注入落进 `about:blank` 上下文 ⇒ **一整排断言全红、全是 helper undefined**；日历常量写死 ⇒ 跨午夜必然变红 |
| 10 | **★ 测到旧二进制（最隐蔽）** | 见 §5.2 |

### 5.2 ★★ 假绿实例：`Copy-Item` 保留 mtime ⇒ cargo 跳过重建 ⇒ 测到旧二进制

**这是本项目踩过两次的坑。**做 A/B 还原验证（「还原修复看测试是否变红」）时：

```powershell
# ❌ 错：Copy-Item 会保留源文件的 LastWriteTime
Copy-Item $backup -Destination <源文件>

# ✅ 对：必须显式把 mtime 顶到「现在」
(Get-Item <file>).LastWriteTime = Get-Date
```

否则 cargo 认为目标比产物**旧**、**跳过重建**，你测的是**上一轮的二进制** ⇒
「还原后测试仍通过」是**假象**，而你据此会得出「这个测试没用」的错误结论。

> ⚠ 与它**配对**的另一个坑：还原时要先确认**这条路径实际会走到哪一行**。
> 真实先例（2026-09-22）：还原的是函数**末尾**那行 `return set[key]`，
> 而该场景下 `set` 是**空的**，代码在**前面**的 `if !ok || len(set) == 0 { return false }` 就返回了
> ⇒ 改的那行**根本没被执行** ⇒「测试通过」⇒ 差点据此认为测试无效。
> **可靠做法是在还原处加一行 `println` 看它是否真的被打印。**

### 5.3 假红也是成本：`cargo` 的 stderr 会被 PowerShell 包成错误记录

`uitest\check-1034.log`（UTF-16LE，842 B）逐字是：

```text
cargo :     Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.23s
At line:1 char:187
+ ...  $env:PATH; cargo check -p ai-gateway-router --all-targets *> <本机日志路径> ...
+                 ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
    + CategoryInfo          : NotSpecified: (    Finished `d...get(s) in 1.23s:String) [], RemoteException
    + FullyQualifiedErrorId : NativeCommandError
```

即 **`cargo check` 成功、EXIT 0**，但 `*>` 把 cargo 写到 **stderr** 的
`    Finished …` 也当成了错误记录 ⇒ 日志看起来像失败。

★ **危害不是「看不懂」，而是它诱导人写 `|| true` / `2>$null` / `$ErrorActionPreference='SilentlyContinue'`
把这段消掉 —— 那一步就把真检查也一起消掉了。**

**正解**：判据只取**退出码**与**产物是否生成**，不看 stderr 是否为空的观感：

```powershell
cargo check --workspace --all-targets --profile test *> $log
$code = $LASTEXITCODE
if ($code -ne 0) { throw "cargo check 失败 EXIT=$code，见 $log" }
# 要断言 warnings，就数日志里 'warning' 开头的行，或直接用 cargo 的 --message-format=json
```

### 5.4 ★ 期望值写死 ⇒ 假红；而「为了消除假红」写宽 ⇒ 假绿

**先红后绿才是有效门禁。** 期望值既不能写死，也不能放水。

| 先例 | 形态 | 处置 |
|---|---|---|
| `verify-hub-real.cjs` 的 `expectMap` 写死 `'trae-checkin': 'done'` | `trae-checkin` **只在「今天已经签过到」时**是 `done` ⇒ 跨午夜或换一个今天还没签到的实例，这条断言**必然变红**，而产品渲染完全正确 | **规矩：真机断言的期望值一律从真机数据推，不许写死。** 做法：读 `trae_checkin_results.json` 的**今日**条目（本机时区，与被测函数同口径）；`zcode-warm` 读 `zcode\accounts.json` 的 `warmedAt` 是否落在 7 天窗口内。**数据读不到、或同一平台多账号推导结果不一致 ⇒ 返回 `null` ⇒ 不断言**（宁可少一条，也不产生假 FAIL）；并且**惰性求值**（断言那一刻才读盘），避免宿主改写当天状态造成假红 |
| `guard-live.ps1` 把网关版本写死成 `1.0.44` | 工作区已 bump 到 `1.0.45` ⇒「版本对不上」被报成 FAIL。**那是期望值过期，不是产品缺陷** | 改成**从 `crates\ai-gateway-router\Cargo.toml` 现读** `^version`，以后 bump 不会再假红 |
| `verify-1028-deployed-gw.cjs` 的落地文件名复算 | 落地文件名里的**长度段是 gzip 载荷长度，不是解压后的 exe 长度**（`gateway_embed.rs` 的「FNV-1a」用的是写错一位的质数 `0x1000_0000_01b3`，只影响文件名） | 断言脚本**照源码口径复算**，别自作主张用解压后长度（曾因此吃一次假红） |

### 5.5 ★ 门禁必须驱动真实路径，且**必须能真的变红**

> **新门禁先做故障注入证明它会失败，才承认它是绿的。**

具体动作（本项目已经在做的两件事）：

1. **故障注入 / 还原验证**：临时改坏被测逻辑 → 观察**变红** → 改回 → **把 mtime 顶到 `Get-Date`**（§5.2）→ 再跑一次确认变绿。
2. **反向对照**（和失败组一样重要）：正常输入**不该被误判**。真实先例——
   `ab_emptystream.rs` 里「孤立的单次空流：不触发风暴、**不罚号**」那一条；
   `mock_upstream.rs` 的 `ok` 场景；1.0.34 的模式 B/D/E。
   ★ 「误判（把正常回答切掉）比漏判更糟」，所以对照组必须存在。

**★ 一个反直觉但很贵的先例（1.0.45）**：全池冷却兜底路径的断言**故意让两个账号的冷却时间不同**
—— 否则「选中的是没被护栏挡的那个」与「恰巧最早的那个」无法区分，测试**看起来绿但什么都没证明**。
**修掉这次假绿后的版本才是有意义的版本。**

**发现假绿按缺陷处理：先修门禁，再谈交付。**

---

## 6. 新增测试 / 探针的检查清单

### 6.1 通用

- [ ] 断言驱动的是**真实路径**（真 socket / 真 HTTP / 真实例），不是桩；**桩只用于「不想碰外部」的那一层**。
- [ ] **故障注入证明过它会失败**（改坏 → 变红 → 改回 → **`(Get-Item f).LastWriteTime = Get-Date`** → 变绿）。
- [ ] **反向对照**断言存在（正常输入不被误判），不只是失败组。
- [ ] **退出码**：全绿 0，有失败**非 0**。没有 `|| true`、没有吞异常、没有 `SilentlyContinue`。
- [ ] **期望值不写死**：版本号从 `crates\ai-gateway-router\Cargo.toml` 现读；日历/时区相关的值从真机数据推；
      推不出就返回 `null` **不断言**。
- [ ] 钉住关键数字（如 `doneCount == 1`、`[DONE]` 出现次数、护栏边界、`max_in_flight`）——
      **改阈值必须让测试变红**。
- [ ] 加了新的集成测试文件 ⇒ **基线表里的「15 个 test 块」也要更新**（§0.2）。
- [ ] 新增/修改的断言在 **`--workspace`** 下至少跑一遍（§1 的 feature 统一只在 `--workspace` 发生）。

### 6.2 涉及假上游

- [ ] 用 `crates/ai-gateway-router/src/testing/` 的 `MockUpstream`（随机端口 `127.0.0.1:0`、**不触网**、
      可断言收到的请求），别在用例里各写一遍一次性假上游。
- [ ] 要**并发**打假上游 ⇒ 不能用单线程的 `MockUpstream`（它没有 `set_read_timeout`，
      并发会卡在 `incoming()` 上被误读成网关死锁）；改用 `tokio::net::TcpListener` + `axum::serve`。
- [ ] `testing/` 子树**必须**保持 `#[cfg(test)]`（否则测试夹具会被打进发行包）。
- [ ] **三种协议各跑一遍**（OpenAI / Anthropic / Responses），别只测 `/v1/chat/completions`。

### 6.3 涉及真机

- [ ] 独立名 + 独立端口 + 独立数据目录 + （GUI）独立 identifier；产物大小对得上
      （15MB ≈ server / 30MB = GUI，别用）。
- [ ] 前后各跑一次 `:7864` / `:3080` 在线自检；收尾按 **PID + 路径双条件**清场并断言残留为 0。
- [ ] `curl` 的 body **用文件传**（§4.3 的引号陷阱）。
- [ ] 跑的是**当次构建**的产物（不是上一轮留下的 exe）。
- [ ] 报告里明确写出「**哪些分支一次都没走到**」（假上游是死的 ⇒ 依赖上游真值的路径验不到）。
- [ ] **绝不动鼠标、不抢焦点、不点桌面** —— 所有者可能正在用这台机器。

### 6.4 涉及入库

- [ ] 不写本机绝对路径、真实密钥、真实账号标识/UID/deviceMid 进**入库**的文件。
- [ ] `uitest/`（证据日志）、`test-bin/`（隔离实例 + `DELIVERY-*.md`）**不入库**；
      `.gitignore` 用 `*.log` 覆盖 `uitest/*.log`，用 `/test-bin/` 覆盖隔离实例。
- [ ] 新增的**测试依赖**（夹具目录、`dist/` 之类）先回答一句：**「CI 上它从哪来？」**（§2.2）。
- [ ] `.ps1` 改了要补 **UTF-8 BOM**（PS 5.1 无 BOM 时按 GBK 读，中文注释会让语法崩），
      并用 `Parser::ParseFile` 验证。

---

## 附：证据与产物都不入库

| 路径 | 内容 | 状态 |
|---|---|---|
| `uitest\` | 实测证据日志（当前 22 个 `.log`，UTF-16LE） | 未跟踪；`.gitignore` 的 `*.log` 覆盖 |
| `test-bin\` | 隔离实例、`DELIVERY-*.md` 交付说明、侦察报告、探针脚本 | `.gitignore:109 /test-bin/` |
| `dist-single\` / `dist-bin\` | 单文件产物 | `.gitignore` |
| `.github\RELEASE_NOTES_*.md` | 1.0.0 → 1.0.46 的发版说明 | **入库**（与 `test-bin\DELIVERY-*.md` 分工不同） |

**不回库的理由**：① 体积（每个隔离实例都有一份 12MB 量级的网关副本）；
② **泄漏面**（侦察报告与探针脚本可能夹带真实账号标识）。

⚠ 代价是：**别人 clone 后拿不到这些探针**，需要复用时照 §4.2 / §6 的约定自己写。
