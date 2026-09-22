<!-- TRELLIS:START -->
# 编码契约（Coding Contracts）

These instructions are for AI assistants working in this project.

本仓库曾由 Trellis 工具初始化。Trellis 的工作流骨架（`workflow.md`、`workspace/`、
`spec/cli/`、`.agents/skills/`、`.codex/agents/`）与 spec 目录均未随仓库分发，
相关引用已于 2026-09-12 清理。

当前生效的编码约定见下方各节（UI Component Policy、Git Commit Language）。

## Shell Policy（强制）

- 本仓库运行环境为 Windows。所有 shell 命令**必须**显式使用 PowerShell（`pwsh` / `powershell`），
  禁止调用 `bash` / `sh` / `zsh` / WSL 及其启动器 `C:\WINDOWS\system32\bash.exe`。
- 调用命令行工具时必须显式指定 shell 为 PowerShell，不要依赖宿主默认值：默认值在
  仅安装了 WSL 启动器、未安装 Linux 发行版的机器上会落到 `bash` 并直接失败。
- 路径一律使用 Windows 形式（`D:\workbuddy2api\...`）、`\` 作为分隔符；环境变量用
  `$env:NAME`。禁止 `curl | sh`、`export VAR=...`、`/dev/null` 等 POSIX 写法。
- 脚本、构建、测试命令一律写成 PowerShell 形式（如 `Get-ChildItem`、`Remove-Item -LiteralPath`、
  `$LASTEXITCODE`）。仓库内已有 `scripts/*.ps1` 的，优先复用而不是重写为 shell 脚本。
- 若某工具确实只提供 POSIX 方式，先确认 `pwsh` 下无等价方案，再在 `AGENTS.md` 记录原因，
  不得静默改用 bash。

Token 统计与网关的接口契约可直接查阅实现本身：

- `crates/ai-gateway-core/src/modules/token_stats.rs` — Token 统计聚合
  （`get_statistics(days: Option<i64>)`）
- `crates/ai-gateway-server/src/api.rs` — HTTP 路由（含 `GET /api/token-stats`）
- `src-tauri/src/commands.rs` — 对应的 Tauri 命令包装

<!-- TRELLIS:END -->

## UI Component Policy

- For frontend UI, prefer the project's existing shadcn components and compose them before writing custom interactive primitives.
- If a required component is missing, add the matching shadcn/Radix component and wrap it under `src/components/ui/` so styling, accessibility, focus management, and behavior stay consistent.
- Write a custom component only when shadcn components and their composition APIs cannot satisfy the requirement. Record the reason before doing so.
- Custom UI must still reuse the project's Rhea theme tokens, spacing, radii, states, and accessibility conventions. Do not substitute native interactive shortcuts such as `details/summary` when an appropriate shadcn component exists.

## 构建与签名（Build & Signing）

构建带 updater 签名的安装包时，**不要再去搜索私钥**，位置与用法如下（2026-09-19 实测校准）：

- 一条命令（**必须显式带上 cargo 的 PATH，见下**）：
  ```powershell
  $env:Path = "C:\Users\iuuuuuuuu\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin;$env:Path"
  & ".\scripts\build-signed.ps1" -KeyFile "D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.key" `
                                -PasswordFile "D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.password"
  ```
  仅校验密钥不构建：加 `-CheckOnly`
- 签名私钥：`D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.key`（minisign 私钥）
- 私钥口令：`D:\WishProject\WorkbuddySwitchAPi\keys\ai-gateway-updater.password`
- 两者在 **`wt-port` 之外、`WorkbuddySwitchAPi` 之内**，`.gitignore` 已排除 `*.key`；
  **本仓库是公开仓库，严禁把口令写入任何被 git 跟踪的文件。**
  （脚本默认找 `%USERPROFILE%\.wb-switch\` 与 `%USERPROFILE%\.ai-gateway\`，
  本机两处都不存在，故**必须**用 `-KeyFile` / `-PasswordFile` 显式指定。）

### ⚠ 三处曾经误导过我的过时信息（已按实测更正）

1. **密钥路径**：原文写 `%USERPROFILE%\.ai-gateway\`，实际在
   `WorkbuddySwitchAPi\keys\`。照原文找会"找不到密钥"。
2. **keyid**：原文写 `217C0E2B4321D841`，**实测是 `7e7ce64b6fc3a36f`**
   （`tauri.conf.json` 的 pubkey 与私钥实际配对的就是它）。
   照原文比对会误判成"密钥不配对"。
3. **`cargo` 不在新 shell 的 PATH 里**，也不在 `~/.cargo/bin`。
   直接跑 `npm run tauri -- build` 会报
   `failed to run 'cargo metadata' ... program not found`，
   而那个报错**完全不提 PATH**，很容易被误判成"Rust 环境坏了"。

   实测（2026-09-21）：`cargo`/`rustc` 实际在
   `C:\Users\iuuuuuuuu\.cargo\bin\`（rustup 1.98.1 装的 stable-msvc），
   把这个目录加进 PATH 即可。**注意**：受限沙箱下 `Test-Path` 对这个目录
   会返回 `False`（访问被拒），据此会误判成"工具链不存在" ——
   用 `cmd /c dir` 或直接执行 `cargo --version` 来确认。

### ⚠⚠ 内嵌网关必须是**当次构建**的那份（2026-09-21 结构性缺陷）

**症状**：新功能在开发机上好好的，装完却没有；构建日志里没有任何异常。

**根因**：`crates/ai-gateway-core/build.rs` 会**无条件内嵌**
`crates/ai-gateway-core/embedded/gateway.exe` 里当时躺着的那一份，
而 `build-signed.ps1` **此前从不构建网关**（只有 `build-single.ps1` 有那段逻辑）。
于是改了 `go-gateway/` 却没手动重建 embedded 时，打出来的包内嵌旧网关。

**现在的约定**：`build-signed.ps1` 的第 `[3/4]` 步会**强制重建**网关
（调用 `scripts/build-gateway.ps1 -Force`），并在构建后做**双端指纹比对**：

- 构建前后 `embedded/gateway.exe` 的 SHA256 必须一致（防并发构建）
- 若设了 `AI_GATEWAY_ROUTER_BIN` 且它指向**另一份**网关，**直接失败**
  （build.rs 的查找顺序里环境变量优先于 embedded/，那会让内嵌的不是你构建的那份）

**单独重建网关**（不打包）：

```powershell
pwsh scripts/build-gateway.ps1            # 增量：源码没变则跳过
pwsh scripts/build-gateway.ps1 -Force     # 强制重建
pwsh scripts/build-gateway.ps1 -CheckOnly # 只校验新鲜度，不构建
```

**快速迭代 UI 时**（确定网关没动）可用 `build-signed.ps1 -SkipGateway` 跳过重建，
省掉约 10 秒 —— 但它**放弃**本次构建对网关新鲜度的保证。

**交付前自检**（解包比对，无需安装）：

```powershell
# 从 target/release/ai-gateway.exe 里解出内嵌网关，比对 SHA256
$want = (Get-Content crates\ai-gateway-core\embedded\gateway.exe.sha256 -Raw).Trim()
# 扫描 gzip 魔数 1F 8B 08，解压后应以 4D 5A (MZ) 开头且 >1MB，再哈希比对
```

背景（改动相关代码前务必了解，否则会重复踩坑）：

- `src-tauri/tauri.conf.json` 的 `createUpdaterArtifacts` **一直为 `true`**，因此
  `tauri build` 必须拿到私钥；`tauri.conf.json` 里的 `plugins.updater.pubkey`
  必须与私钥配对（keyid `7e7ce64b6fc3a36f`），否则客户端会拒绝更新包。
- `TAURI_SIGNING_PRIVATE_KEY` 的值必须是密钥**内容**，不是路径。传路径会报
  `failed to decode base64 secret key: Invalid symbol 58`（路径里的冒号）。
  读取方式：`(Get-Content -LiteralPath $key -Raw).Trim()`。
- **缺口令时 `tauri signer sign` 不报错，而是阻塞等待 stdin 输入**，表现为"构建卡住"；
  口令错误才会秒级报错。所以非交互场景务必先确认口令可用（`-CheckOnly` 就是为此）。
- 加密私钥的 keynum 偏移（54..62）与公钥（2..10）不同，**不能直接比对**；
  判断配对是否正确的唯一可靠方式是实际签一次，再比签名与公钥的 keyid。

## 验证与测试（强制，血泪教训）

所有者本机**正在运行这套软件**：`:7864` 是他的网关，`:43120` 是他运行 AI 对话的
DSH Desktop。**任何验证都不得影响这两个进程。**

### 绝对禁止：运行安装包 / 卸载器

包括 `AI-Gateway_*_setup.exe` 与 `uninstall.exe`，**加了 `/S` 更危险**。

原因：Tauri 的 NSIS 模板按**可执行文件名**匹配并结束进程，不比对路径：

```nsis
nsis_tauri_utils::FindProcessCurrentUser "${executableName}"
nsis_tauri_utils::KillProcessCurrentUser "${executableName}"
IfSilent kill_${UniqueID} 0        ; 静默模式下不询问，直接杀
```

主程序名 `ai-gateway.exe` 与所有者正在运行的实例**同名**，因此
「装一次 / 卸一次」就会杀掉他的实例；而它是 Job Object（`KILL_ON_JOB_CLOSE`）
的父进程，父进程一死，`:7864` 网关被系统连带回收 —— 一次误操作打掉两个服务。

**验证安装包内容请用解包**：内嵌网关是 exe 内的一个 gzip 流，
定位 `1F 8B 08` 魔数后解压即可与源码编译结果逐字节比对，无需安装。

### 绝对禁止：按进程名批量结束进程

```powershell
# 禁止
Get-Process -Name "gateway*" | Stop-Process
```

清理只允许**PID + 路径双条件**：

```powershell
$proc = Get-CimInstance Win32_Process -Filter "ProcessId=$pid" -ErrorAction SilentlyContinue
if ($proc -and $proc.ExecutablePath -like "*test-bin*") { Stop-Process -Id $pid -Force }
```

### 验证一律用独立实例：独立名 + 独立端口 + 独立数据目录

**陷阱**：`crates/ai-gateway-server`（server，约 15MB）与 `src-tauri`（GUI，约 30MB）
的 `[[bin]] name` **都叫 `ai-gateway`**，输出到同一个 `target\release\ai-gateway.exe`
互相覆盖。直接用它做接口测试可能拿到 **GUI** —— GUI 带单实例插件，
发现所有者的实例在跑就**自己静默退出**（无输出、不监听），
极易误判成「代码启动即退出」。

```powershell
# 1) 用独立 target 目录构建 server 版，避免覆盖 GUI 产物
$tdir = "D:\WishProject\WorkbuddySwitchAPi\test-bin\build-server"
cargo build --release -p ai-gateway-server --target-dir $tdir
# 产物约 15MB = server 版；30MB 就是 GUI，别用

# 2) 改名，确保与所有者的进程都不同名
Copy-Item "$tdir\release\ai-gateway.exe" "D:\WishProject\WorkbuddySwitchAPi\test-bin\wb2api-server.exe"

# 3) 独立数据目录 + 独立端口（避开 7864 / 43120 / 57890，用 5789x）
$home1 = "D:\WishProject\WorkbuddySwitchAPi\test-bin\inst-xxx"
New-Item -ItemType Directory -Force -Path $home1 | Out-Null
$env:AI_GATEWAY_HOME = $home1
$p = Start-Process -FilePath "...\wb2api-server.exe" -ArgumentList "serve","--port","57899","--no-open" `
    -PassThru -WindowStyle Hidden -RedirectStandardOutput "$home1\o.log" -RedirectStandardError "$home1\e.log"
"PID=$($p.Id)" | Out-File "$home1\pid.txt" -Encoding ascii
```

Go 侧的 `gateway.exe` 不存在上述覆盖问题，可直接构建到临时目录使用。

### 前端/UI 验证不需要起后端

用 `uitest/page-harness.cjs` + `uitest/mock-host-api.cjs`
（静态服务 + mock 宿主 API + CDP 驱动真实 Chrome）。

### 每次验证前后都自检

```powershell
foreach ($port in @(43120, 7864)) {
    $c = Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue
    if (-not $c) { Write-Warning "所有者的 :$port 未在运行 —— 立即停止并报告" }
}
```

详见 `uitest/README-验证约定.md`。

### ⚠⚠ 自己打的包要自己跑通再交付（2026-09-21 教训）

所有者原话：

> 「你自己打包后自己测试啊，你不会吗？」

**对**。此前我把"编译通过 + 单元测试绿 + 解包比对 SHA256"当成了交付标准，
而这三项**都证明不了功能可用**。实际交付的包里 WebView2 求解**从未成功过**，
却因为**本地 Node 兜底**而表现为"能用" —— 我在自己的机器上看到 200 就以为成了。

**判定"功能是否真的生效"必须绕过兜底路径。** 具体到 ZCode 验证码：

```powershell
# 直接调宿主求解服务（那条路没有兜底），而不是发对话请求
curl.exe -s -X POST "$solver_url`?scene=11xygtvd&region=cn&prefix=no8xfe" `
  -H "X-Captcha-Token: $token" -w "`n[HTTP %{http_code}]"
# 成功 = {"param":"...280 字符...","region":"cn"}；失败 = {"error":"..."} 或超时
```

发对话请求**不能**作为判据：网关的实现是"外部求解失败 → 回退本地 Node"，
于是外部全坏时，只要本机装了 Node，对话照样成功。

**要跑 GUI 侧的代码，用改 identifier 的独立构建**（不改产品文件）：

```powershell
# override.json: {"identifier":"com.test.warmup.probe","bundle":{"createUpdaterArtifacts":false}}
npx tauri build --no-bundle --config "<路径>\override.json"
```

独立 identifier ⇒ 独立单实例锁 + 独立 WebView2 数据目录 ⇒ 与所有者的实例并存互不干扰。
配 `AI_GATEWAY_HOME` 指向独立目录，端口也换掉（如 57899）。

**GUI 的 stdout/stderr 不进 PowerShell 管道** —— 必须显式重定向才看得到日志：

```powershell
Start-Process -FilePath "<exe>" -PassThru -WindowStyle Hidden `
  -RedirectStandardOutput "$log\out.log" -RedirectStandardError "$log\err.log"
```

### ⚠ 两个已验证的 Tauri 陷阱（WebView2 求解实现踩过）

1. **capability 的 `windows` 白名单会静默拦下非 `main` 窗口的 IPC。**
   `capabilities/default.json` 原本只有 `"windows": ["main"]`，而隐藏的求解窗口
   叫 `captcha-solve-{id}` → 页面里的 `invoke` 被拒且**不抛到宿主**，
   表现为必然超时。新增窗口 label 时要同步加进白名单。

2. **`document.write` 会冲掉 `initialization_script` 注入的变量。**
   用 `WebviewUrl::External("http://127.0.0.1/")`（**没有服务器在听**）+ eval
   灌 HTML 的路子不可靠：导航失败与 eval 是竞态，且 `document.write` 替换整个
   文档。识别特征是**窗口创建耗时异常短**（实测 0.5~7ms，真实建 WebView2 要
   几十~几百毫秒）—— 说明只是登记了窗口，页面没跑。
   现方案：页面由宿主自己的 HTTP 服务提供（参数走 URL，结果用**同源 fetch** 回报）。

3. **`#[serde(flatten)]` + `#[serde(default)]` 会吞掉结构不匹配。**
   页面发 `{"id":1,"result":{...}}`（嵌套），而结构体写 `flatten` 期望扁平
   → `id` 解析成功、其余字段**静默取默认空值**，不报任何错。
   表现为 `param长度=0 region= error=（无）`，看起来像"SDK 给了空值"。
   跨语言报文要**对着实际发送方**写，别凭"看起来应该扁平"推断。

## ⚠⚠ 公开仓库：不要把真实账号标识写进代码（2026-09-21）

**本仓库是公开仓库。** 排查线上问题时很容易顺手把真实值粘进注释或测试夹具，
本轮就发生过：`cred.go` 的注释、`device_mid_*_test.go` 的夹具、
`usage.go` / `identity.go` 的示例、Rust 侧的 doc 注释里，
一度散落着**真实的账号 UUID、deviceMid、上游账号 ID**。

**交付前必扫**（把这些模式全部替换成明显的占位值）：

```powershell
# 换成你实际用过的值；命中即清理
$pats = @('<真实账号UUID>','<真实deviceMid>','<真实上游账号ID>','<凭证uid前缀>')
foreach ($p in $pats) {
  Get-ChildItem . -Recurse -File -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -notmatch 'node_modules|\\target\\|\\dist\\|\\.git\\|test-bin' -and $_.Length -lt 5MB } |
    Select-String -Pattern $p -SimpleMatch -ErrorAction SilentlyContinue |
    ForEach-Object { "$($_.Path):$($_.LineNumber)" }
}
```

占位值用**一眼能看出是假的**形式：`{uuid}`、`12345678901234567`、
`00000000-0000-4000-8000-000000000001`。

⚠ **改完必须重跑测试**：占位值若参与了断言（如 UUID 形态校验），
换成非法形态会让测试失败 —— 那正是它该失败的时候。

## ⚠ 参考实现的注释是**二手信息**，本仓库的抓包才是一手证据（2026-09-21）

排查 ZCode 3012 时，参考实现里有一条注释看起来**完美解释**了问题：

> 「start-plan（JWT 通道）只发 3 个头，**不发** `x-query-id` / `x-session-id`。
>   **误发会触发 3012**」

我据此改了代码。但核对本仓库 `go-gateway/internal/zcode/CAPTURED-SPEC.md`
（由 Reqable 抓到的**成功请求**生成）后发现：官方客户端在 start-plan 端点上
**确实发了**这两个头。**那条注释是错的**，改动全部撤销。

`CAPTURED-SPEC.md` 开头就写着：

> 这是**唯一可信**的规格来源 —— 此前所有关于该通道的结论都是推断，**且多次出错**。

**规则**：改动上游协议细节前，先读 `CAPTURED-SPEC.md`；
参考实现的注释可能过时或针对别的通道/版本，不能当作约束。

## ⚠⚠ `.ps1` 必须带 UTF-8 BOM，且别用字符串替换改它（2026-09-21）

**症状**：脚本突然报一堆语法错，位置看起来毫无道理：

```
行 37: Missing expression after ','.
行 63: Unexpected token '}' in expression or statement.
行 101: The string is missing the terminator: "@.
```

**根因**：`build-signed.ps1` 里有大量中文注释。**PowerShell 5.1 在没有 BOM 时
按 ANSI（本机 GBK）读取 `.ps1`** —— 中文全乱码，引号与花括号的配对随之崩掉。

```powershell
# 正确：UTF-8 **带 BOM**（239,187,191）
$b = [System.IO.File]::ReadAllBytes($path)
if ($b[0] -ne 239) {
    $out = New-Object byte[] ($b.Length + 3)
    $out[0]=239; $out[1]=187; $out[2]=191
    [Array]::Copy($b, 0, $out, 3, $b.Length)
    [System.IO.File]::WriteAllBytes($path, $out)
}
```

**触发方式（我踩了两次）**：

1. 用 `[System.IO.File]::WriteAllText($p, $t, UTF8Encoding($false))` 改脚本
   —— `$false` 表示**不要 BOM**，直接把 BOM 抹掉
2. 用 `edit` 工具改这个文件 —— 它也会去掉 BOM

**改这个脚本的规矩**：

- 优先用 `edit` 工具改**内容**，改完**必须补 BOM**（上面的片段）
- 改完**必须**用 `Parser::ParseFile` 验证（它处理 BOM，`ParseInput` 不行）：

```powershell
$err = $null
[System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$null, [ref]$err) | Out-Null
if ($err.Count) { $err | Select-Object -First 3 | ForEach-Object { "行 $($_.Extent.StartLineNumber): $($_.Message)" } }
```

- ⚠ **别用 `git checkout HEAD -- <脚本>` 回退**：那会把你**未提交**的改动
  一起丢掉（我因此丢过"构建内嵌网关"整段逻辑）。
  误删后可从悬空对象找回：

```powershell
git fsck --lost-found                      # 找 dangling blob
git cat-file -p <blob> | Select-String "构建/刷新内嵌网关"   # 确认是那一版
cmd /c "git cat-file -p <blob> > scripts\build-signed.ps1 2>nul"   # 原样写回
```

## ⚠⚠ 校验"内嵌前端是不是当次构建"只能用**资源文件名**（2026-09-21）

**事故**：所有者报告「zcode 这里还是老卡片啊 你是不是没打包？」。
查证后代码与安装包**都是对的**，但他装的是更早那一版 ——
而我用"在 exe 里搜前端代码里的字符串"作判据，得出**错误结论**，
白绕了一大圈。

**为什么那个判据是错的**：Tauri 对 JS **内容**做了压缩/编码，
`product-usage-bucket` 这类标识在 exe 里**搜不到** ——
连**早就存在**的老标识 `product-usage-bar` 也搜不到。
搜不到 ≠ 没打包。

**可靠判据（已固化进 `build-signed.ps1` 的 `==> 校验内嵌前端指纹`）**：

```powershell
# dist/index.html 写死了它引用哪个 assets/index-XXXX.js
$want = [regex]::Match((Get-Content dist/index.html -Raw), 'assets/index-[A-Za-z0-9_\-]+\.js').Value
# 那个名字是 Vite 按**内容哈希**生成的，且文件名在 exe 里是**明文**
$got = [regex]::Matches([System.Text.Encoding]::ASCII.GetString(
    [System.IO.File]::ReadAllBytes('target/release/ai-gateway.exe')), 'assets/index-[A-Za-z0-9_\-]+\.js') |
    ForEach-Object { $_.Value } | Sort-Object -Unique
$got -contains $want    # True = 内嵌的是当前前端
```

**背景**：`tauri-build` 的 build script 里 `rerun-if-changed` **不包含**
`../dist`，故前端重建**不会**触发重新嵌入 —— cargo 可能沿用旧前端。
这与"内嵌旧网关"是**同型**缺陷。脚本里那道校验就是为此加的闸。

## ⚠⚠ "宿主写配置" 与 "网关读配置" 的时序陷阱（2026-09-22）

**症状**：发**裸名**模型（如 `Qwen3.8-Flash`）被路由到错的平台并回
`400 11101`（那是 **WorkBuddy** 的错误码），而带平台前缀
（`qoder:Qwen3.8-Flash`）就正常。

**根因**：`product_models`（网关用来判断"哪个产品声明提供该模型"的清单）
是在 `start_gateway` 写配置时算的 —— 而那一刻 qoder / zcode 账号库里的
`models` **还是空的**（要靠启动 90 秒后的巡检去上游查）。
巡检把 `models` 写进了账号库，**却没有重写网关配置** ⇒ 清单永远是空的。

**规矩**：任何**改变 `product_models` 输入**的操作（刷新账号、导入账号、
账号巡检）都必须跟着调一次 `gateway::resync_native_config()`。

已接的位置：
- `qoder_login::refresh_account` / `zcode_login::refresh_account`（用户手点刷新）
- `multi_product_credit_patrol::run_once`（后台巡检，2026-09-22 补）

**为什么不在启动时"等账号刷新完再写配置"**：那会让启动变慢，
且巡检可能因网络失败 —— 配置写入**不该依赖外部请求成功**。
resync 是幂等的，多写几次的代价可忽略。

**为什么只写文件、不重启网关**：重启会掐断正在进行的对话。
代价是清单对**已在运行**的网关要等下次重启才生效 —— 这个取舍要写进
给用户的说明里，不要让用户以为"装完立刻生效"。

## ⚠⚠ "兜底清单"不能参与"声明"竞争（2026-09-22）

**症状**：`GLM-5.3` / `Auto` 这类**多个平台都提供**的模型报
`400 model_not_in_region`，而只被单一平台提供的模型（`Qwen3.8-Flash`）正常。
**差别就在"重叠"**。

**根因**：`productDeclaresModel` 一开始只看宿主透传的清单（可信）。
后来为了修"裸名模型被路由到 WorkBuddy"（2026-09-21），
在 `cmd/server/main.go` 里**补了一份内置静态表作为 workbuddy 的兜底**，
而当时的注释写着：

> 它是"声明"用的，不是"否定"用的 —— 多列几个不会让任何账号失去资格

**那句话在引入 `declared` 优先逻辑之后就失效了** —— 因为
`pickForModelAny` 会"优先只在明确声明的产品里挑"，兜底清单一进
`declared`，workbuddy 就与真正提供该模型的平台**竞争**，
而它账号多 19 倍 ⇒ 压倒 ⇒ 选中其实没有该模型的账号。

**规矩**：清单分两份，语义严格区分 ——

| 字段 | 来源 | 参与 |
|---|---|---|
| `productModelSet` | 宿主透传（上游**真实查询**） | 「不排除」**+**「声明」 |
| `productModelFallbackSet` | 网关内置静态表（**猜测**） | **只**参与「不排除」 |

即「不知道某产品提供什么 ⇒ **别排除**它，但也**别声称**它提供」。

⚠ 加任何"补全清单"的逻辑前，先问：**它会被当成声明吗？**
若会被当成声明，就必须走 fallback 那条路 —— 猜测永远不该赢过事实。

## ⚠ 验证"测试能不能抓到 bug"时，要确认**改到了真正生效的分支**（2026-09-22）

我按惯例做"还原修复看测试是否变红"，**第一次做错了**：

- 还原的是函数**末尾**那行 `return set[key]`
- 而该场景下 `set` 是**空的**，代码在**前面**的
  `if !ok || len(set) == 0 { return false }` 就返回了
- ⇒ 我改的那行**根本没被执行** ⇒ 测试"通过" ⇒ 我差点据此认为测试无效

**教训**：还原时要先确认**这条路径实际会走到哪一行**。
可靠做法是在还原处加一行 `println` 看它是否真的被打印。

## ⚠⚠ 流式响应里 `finish_reason` 只能出现一次（2026-09-22）

**症状**：**内容已经出来了**（模型答对），但界面一直转圈，随后报错重试。
所有者现场：「**zcode 没问题、qoder 不行**」。

**根因**：`qoder/stream.go` 的收尾逻辑**无条件**补一个 `finish_reason: "stop"`
的收尾帧，而 qoder 上游**自己也会**在最后一个内容分片里给
`finish_reason`（代码里原样转发）⇒ **同一条流里出现两次**。

实测（同一时刻、同一网关）：

| 模型 | `finish_reason:stop` 次数 |
|---|---|
| `qoder:Qwen3.8-Flash` | **2** ❌ |
| `zcode:GLM-5.3-Flash` | 1 ✅ |

**规矩**：转发上游的 `finish_reason` 时记下"已经给过"，
收尾时**只在没给过时**才补。但仍要保留补帧路径 ——
上游只发内容、始终不给 `finish_reason` 时，
不补会让客户端**一直等**（那是 `finishFrame` 原本要修的缺陷）。

⚠ 只有**非空**的 `finish_reason` 才算"给过"：
上游偶尔发 `"finish_reason":""` 的空帧，把它当结束信号会让客户端一直等。

**排查这类"只有某个平台不行"的方法**：把两者的**帧结构并排 diff** ——
我数出 `finish_reason` 一个 2 次一个 1 次，根因立刻清楚。
比读代码猜快得多。

## ⚠⚠ 交付前必须**自己跑实测**（2026-09-22 所有者强制要求）

所有者原话：

> 「你就不能自己跑一下实测吗?非要让我测试」
> 「每次完成需求或者修复bug之后,必须强制跑实测,才能交付给我」

**"实测"的定义**（三者缺一不可）：

1. **真实启动**一个新构建的实例（独立端口 + 独立数据目录）
2. **真实发请求**走完整链路（`curl` 打到 `/v1/chat/completions`，
   不是只跑单测）
3. 看到**期望的输出**（200 + 正确内容；或明确的、可解释的错误）

**以下都**不**算实测**，不得据此交付：

- 编译通过 / `cargo check` / `go build`
- 单元测试全绿（它们证明不了"打包后能跑"）
- 产物 SHA256 与源码比对一致
- "我在自己的实例上看到 200" —— 除非那是**当次构建**的产物

⚠ **特别注意 `curl` 的引号陷阱**：PowerShell 用单引号包 JSON 会**吃掉内部双引号**，
请求体变成 `{model:xxx,...}`（非法 JSON），上游报
`11101 invalid character 'm'`。**看起来像网关 bug，实际是测试方法错了**。
一律用文件传：

```powershell
$json = '{"model":"qoder:Qwen3.8-Flash","messages":[{"role":"user","content":"hi"}],"stream":false}'
[System.IO.File]::WriteAllText("$tmp\r.json", $json, (New-Object System.Text.UTF8Encoding($false)))
curl.exe -s -X POST "http://127.0.0.1:$port/v1/chat/completions" `
  -H "Authorization: Bearer sk-123" -H "Content-Type: application/json" `
  --data-binary "@$tmp\r.json" --max-time 60 -w "`n[HTTP %{http_code}]"
```

**提交/推送同样要等所有者明确要求**（他确认实测通过后才会说）——
不得因为"我实测过了"就自行 commit/push。

## ⚠⚠ 网关重启必须走串行化入口（2026-09-22）

**症状**：点「重启」提示**「网关已在运行」**，而实际**没有重启**。

**根因**：多处调用方各自写 `stop_gateway()` + `start_gateway()`，
而 `start_gateway` 开头是 `if is_running() { return Err("网关已在运行") }`。
两个调用方交错时，后到的那个**必然**撞上这个检查：

```
后台线程：stop_gateway()   ← 关掉旧的
用户线程：stop_gateway()   ← 关了空 slot（no-op）
后台线程：start_gateway()  ← 起新的，GATEWAY_RUNNING = true
用户线程：start_gateway()  ← 看到 true ⇒ 返回「网关已在运行」
```

**规矩**：任何"重启网关"都必须调 `gateway::restart_gateway_serialized()`，
不要自己写 stop+start。它用 `tokio::sync::Mutex` 把整段串行化。

已统一走的调用点（6 处）：Tauri `restart_gateway`、WebUI `/api/gateway/restart`、
`apply_pending_restart`、`sync_and_reload`、`set_allowed_models`、
`set_model_platforms`、`switch_mode`。

⚠ 用 `tokio::sync::Mutex` 而非 `std::sync::Mutex`：临界区含 `.await`
（等端口就绪最多 20s），`std` 的锁跨 await 会阻塞 runtime 且编译不过。

有测试钉住"不得新增绕过锁的裸 `stop_gateway()`"
（`gateway_restart_goes_through_serialized_entry`）。

## ⚠⚠ 熔断不是"停服"：单账号产品必须兜底（2026-09-22）

**症状**：`qoder:` 前缀的请求**全部 503**，而 `zcode` 与账号名都正常。

**根因**：`applyErrorPolicy` 把上游 5xx 记为 `NoteError`，
连续 3 次触发熔断 **30 分钟**。而产品前缀请求的候选集**锁死了 product**，
`pickStrict` 又刻意不兜底 ⇒ 该产品只有 1 个账号时**彻底不可用**。

**规矩**：熔断的语义是「这个号可能不好，**换个号**」——
当该产品**没有别的号可换**时，熔断只是把一次**上游瞬时故障**放大成停服。
`PickForModelProductRegion` 必须在"该产品一个健康号都没有"时退到
**同产品内部**的冷却兜底。

⚠ **兜底绝不能跨产品**：`qoder:` 是用户的显式指令，把 WorkBuddy 账号
拿给它用会拿错凭证、打到错的端点。有测试钉住这两条
（`single_account_breaker_test.go`）。

⚠ 多账号产品的熔断保护**不受影响**：只要有一个健康号，严格路径就返回了，
兜底分支根本不执行。

## ⚠⚠ 客户端并发重试 vs 单账号产品的并发上限（2026-09-22）

**症状**：**同一个模型、同一个网关** —— 用 `curl` 发**每次都成功**，
但客户端**每次都失败**（`503 no_healthy_account`）。

**根因**：客户端的 LLM 层会**并发重试**。DSH 用的
`@earendil-works/pi-ai` 带 `retryProviderRequest`，默认**重试 5 次**
（`openai-completions.js:213`），**每个重试是独立的并发请求**。

而 `max_in_flight` 是**每账号**上限，qoder / zcode **各只有 1 个账号**
⇒ 该产品的总并发 = 这个上限。曾只有 3 ⇒ 5 次重试里后 2 次必然 503。

**排查这类问题的方法**（值得记住）：

1. **先怀疑"请求形态不同"** —— 读客户端的源码看它到底发什么
   （DSH 在 `%LOCALAPPDATA%\Programs\DSH Desktop\resources\app\node_modules\
   @earendil-works\pi-ai\dist\api\openai-completions.js`）
2. **再怀疑"并发"** —— 客户端带重试，而重试往往是并发的
3. **用并发梯度定位边界** —— 我实测出「并发 3 全通、并发 4 起失败」，
   直接把 `max_in_flight=3` 钉死为根因

**规矩**：

- 任何"每账号"的上限，在**单账号产品**上就变成了"每产品"上限 ——
  设计时必须问「这个产品只有一个号时会怎样」
- 加并发上限这类魔数时**必须写注释说明取值依据**，
  否则后人只看到 `3`，既不知道从哪来，也不敢改
- 客户端的重试次数是**并发度**的信号：DSH 是 5，故上限至少该留
  一个数量级的余量

**当前值**：`max_in_flight = 3`（三个产品统一；2026-09-22 从临时的 32 收回来，
因为真正的解法是下面那条"排队"而不是把上限开大），
配合 `server.Config.InFlightWait`（默认 **3000ms**）在名额满时排队而非失败。

⚠ **本节曾写 `32` / `1500ms`，与实际代码不符**（代码是 3 / 3000ms）。
改魔数时务必同步本节 —— 文档与代码分叉比没有文档更糟。

## Git Commit Language

- Use Conventional Commit type prefixes such as `feat:`, `fix:`, and `docs:`.
- Write the commit subject and body in Chinese by default. Use English only when the user explicitly requests it.
