# 开发入口（AGENTS）

> 面向在本仓库动手的人（含 AI agent）。**开工前读完本页。**
> 最后核对：2026-10-15 ｜ 分支 `main` ｜ 版本 **1.0.46** ｜ 核对时 HEAD `dc729e9`
> 本页是 `docs/` 里的**开发入口**，与仓库根 [AGENTS.md](../AGENTS.md)（编码契约硬约束）**并存**：
> 根那份是既有契约、**不要改**；本页负责「开工第一件事」的收口与索引。
> 改命令 / 约束 / 流程时同步更新本页。

## 0. 一句话

WorkBuddy（CodeBuddy）多账号切换工具 + 本地 AI 网关：桌面 GUI 管账号，内嵌一个 **Rust 网关**对外提供 OpenAI / Anthropic 兼容接口。

本页只写**代码里读不出来的东西**（why、约束、陷阱、验收标准、非显然步骤）。**不复述代码结构** —— 结构会变，读代码比读文档快。

---

## 1. 仓库结构

| 目录 | 是什么 | 产出 / 说明 |
|---|---|---|
| `crates/ai-gateway-core/` | 宿主逻辑库（Rust）。**不依赖 Tauri**，被 GUI 与 HTTP 服务共用 | rlib；`build.rs` 负责构建期压缩内嵌网关 |
| `crates/ai-gateway-router/` | **网关内核**（2026-10 从 Go 全量重写为 Rust） | `[[bin]] name = "gateway"` → `target/<profile>/gateway.exe` |
| `crates/ai-gateway-server/` | HTTP / WebUI 服务形态（npm 分发的那个 `ai-gateway` 命令） | `[[bin]] name = "ai-gateway"` ⚠ 见 §3.5 |
| `src/` | React 前端（Vite） | `npm run build` → `dist/` |
| `src-tauri/` | Tauri 2 桌面壳，托管网关进程、托盘、单实例 | `[package] name = "ai-gateway"` ⚠ 与上表同名 |
| `scripts/` | 构建 / 发版脚本（`.ps1` 为主，见 §2.3） | 见 §2 |
| `go-gateway/` | **Go 参照实现，已不参与构建**（仅作对照；`build-gateway.{sh,ps1}` 不再调 `go build`，CI 里的 `actions/setup-go` 已删除） | 改协议细节前仍值得读它的 `CAPTURED-SPEC.md`（见 §4.1） |
| `docs/` | 本目录：**怎么干活** | 见 [README.md](README.md) 文档地图 |
| `.github/workflows/` | `ci.yml`（push/PR 编译门禁）/ `build.yml` / `release.yml`（`v*` tag） | 见 §5.5 |
| `uitest/` `test-bin/` | 本机实测证据与隔离实例 | ⚠ **都不入库**（`.gitignore`），见 §5.6 |

Cargo workspace 成员：`crates/ai-gateway-core`、`crates/ai-gateway-router`、`crates/ai-gateway-server`、`src-tauri`。

---

## 2. 环境与命令

### 2.1 Shell（强制）

- **一律 PowerShell**。禁 `bash` / `sh` / `zsh` / WSL 及其启动器。路径用 Windows 形式与 `\`；环境变量用 `$env:NAME`；禁 `curl | sh`、`export VAR=...`、`/dev/null` 等 POSIX 写法。
- 仓库已有 `scripts/*.ps1` 的，**优先复用**，不要重写成 shell。

> ⚠ **本机实测（2026-10-15）：`pwsh` 并未安装。** `Get-Command pwsh` → 未找到；本机只有 Windows PowerShell **5.1.19041.6456**。
> 因此文档 / 脚本里写的 `pwsh scripts/...` 在本机直接报「无法将 pwsh 项识别为 cmdlet」。
> 变通：在已有 PowerShell 会话里执行 `& .\scripts\build-gateway.ps1`，或 `powershell.exe -NoProfile -File <脚本>`。
> （`README.md` 与根 `AGENTS.md` 建议「优先用 `pwsh`」—— 在装了 PS 7 的机器上成立，本机不成立。**别据此判定脚本坏了**。）

### 2.2 Rust 工具链与 cargo 的 PATH

`cargo` 历史上不在新 shell 的 PATH 里，直接跑 `npm run tauri -- build` 会报
`failed to run 'cargo metadata' ... program not found` —— 该报错**完全不提 PATH**，极易误判成「Rust 环境坏了」。

```powershell
# 通用写法（$env:USERPROFILE 拼路径，勿写死用户名）
$env:Path = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin;$env:Path"
```

**两点必须知道**：

1. `%USERPROFILE%\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin\cargo.exe` 是**真正的 cargo**（约 27MB，2026-10-15 实测 `cargo 1.99.0`）；
   而 `%USERPROFILE%\.cargo\bin\cargo.exe` 是 **rustup 的 shim**（符号链接到 `rustup.exe`）。前置哪个都能跑，但排查工具链问题时别把两者混为一谈。
2. **2026-10-15 实测更正**：`%USERPROFILE%\.cargo\bin` **已经**在 User 与 Machine 的 PATH 里，新的 `powershell.exe -NoProfile` 会话**能**解析 `cargo`。
   即「cargo 不在 PATH」这条（2026-09-21 记录）**在当前机器上已不成立**。
   但**仍建议显式前置**：零成本，且能挡住换机器 / 换 CI 时的环境差异。CI 上 cargo 由 `dtolnay/rust-toolchain@stable` 提供。

> ⚠ **受限沙箱下 `Test-Path` 对 `.cargo` 目录会返回 `False`（访问被拒）** ⇒ 据此会误判成「工具链不存在」。
> 用 `cmd /c dir` 或直接执行 `cargo --version` 确认。

### 2.3 ★ 绝不加 `--offline`

**本机 cargo 的 registry 缓存是空的**，加 `--offline` 会直接
`error: no matching package named 'aes' found`。**联网正常构建**即可。

> ⚠ **2026-10-15 复测：原始理由的「缓存是空的」这一句已不成立**，但**规矩保留**。
> 实测 `%USERPROFILE%\.cargo\registry\cache\` 下有两个 source 目录
> （`index.crates.io-…` 1064 个 `.crate`、`rsproxy.cn-…` 553 个），
> `aes-0.8.4.crate` **在**（可复现：`ls "$env:USERPROFILE\.cargo\registry\cache\*\aes-0.8.4.crate"`）。
> ⇒ 本机现在加 `--offline` 未必立刻报错。
>
> **为什么仍不许加**：
> ① **CI 与别人的机器没有这份缓存**（`~/.cargo` 不入库）—— 门禁必须与 CI 同形，
>    本地能过、CI 不能过的命令是假门禁的温床；
> ② 缓存是**历史积累**，新依赖照样解析不出来；
> ③ `--offline` 会把「该联网」的失败伪装成「包不存在」，把网络问题误诊成依赖问题。
> 真要离线构建，须先 `cargo fetch` 把依赖拉全，并明确知道自己在做什么。

镜像来源是 `wt-port\.cargo\config.toml`：

```toml
[source.crates-io]
replace-with = "rsproxy"
[source.rsproxy]
registry = "sparse+https://rsproxy.cn/index/"
```

- **为什么必须有它**：本机到 crates.io 的 TLS 握手会失败（`SSL connect error (schannel: failed to receive handshake)`），表现是「加了一个完全正常的依赖，本地却永远解析不出来」。
- **为什么是 `sparse+`**：git 协议索引会整仓克隆（数百 MB），sparse 只按需拉单个 crate 元数据。
- ⚠ **这个文件被 `.gitignore:25` 排除，不入库** ⇒ 新 clone 的机器 / CI **拿不到镜像**，走的是官方 crates.io。
  遇到上面那个 TLS 报错时，需要**自己重建**这份 `.cargo/config.toml`。
- `Cargo.lock` 里同时出现 `rsproxy.cn` 与 `index.crates.io` 两个源是**正常且期望**的：镜像只是「取包的通道」，版本仍由 `Cargo.lock` 钉死。

### 2.4 常用命令

| 用途 | 命令 |
|---|---|
| 前端类型检查 + 构建 | `npm run build`（= `tsc && vite build`） |
| 依赖安装 | `npm ci` |
| 全量单测（**主门禁**） | `cargo test --workspace` |
| 网关内核单测 | `cargo test -p ai-gateway-router` |
| 宿主逻辑库单测 | `cargo test -p ai-gateway-core` |
| 编译校验（与 CI 同款） | `cargo check --workspace --all-targets --profile test` |
| 构建网关（单独） | `pwsh scripts/build-gateway.ps1` / `-Force` / `-CheckOnly` |
| 签名安装包 | `scripts/build-signed.ps1`（见 §2.5） |

### 2.5 签名安装包（`build-signed.ps1`）

`createUpdaterArtifacts` **恒为 `true`**（`src-tauri/tauri.conf.json` 实测确认）⇒ `tauri build` **必须**拿到私钥，否则客户端拒绝更新包。

**脚本参数**：`-Bundles`（默认 `"nsis"`）/ `-KeyFile` / `-PasswordFile` / `-CheckOnly` / `-SkipGateway`。

**四个步骤**（不是三个），每步失败即中止：

| 步骤 | 做什么 |
|---|---|
| `[1/4]` | 检查签名私钥存在、非空、且首行是 minisign 私钥 |
| `[2/4]` | **实际签一次**校验口令与密钥配对 |
| `[3/4]` | **强制重建内嵌网关**（`build-gateway.ps1 -Force`）并做双端 SHA256 指纹比对 |
| `[4/4]` | `tauri build --bundles <Bundles>`，产出安装包与 `.sig` |

**密钥位置（★ 与脚本默认探测路径不同）**：

- 脚本 `Resolve-KeyPath` 的首选是 `%USERPROFILE%\.wb-switch\wb-switch-updater.key`，回退是 `%USERPROFILE%\.ai-gateway\ai-gateway-updater.key`。
- **本机这两处都没有文件**（2026-10-15 实测）⇒ **必须显式传参** `-KeyFile` / `-PasswordFile`。
- 实际位置在**仓库之外、仓库根的同级 `keys\` 目录**（即 `Split-Path -Parent $repoRoot` 下）：
  `ai-gateway-updater.key`（348 B）/ `ai-gateway-updater.password`（40 B）/ `ai-gateway-updater.key.pub`。
- ⚠ 本仓库是**公开仓库**，`.gitignore` 已排除 `*.key` / `*.key.pub`。**严禁把口令写进任何被 git 跟踪的文件**；**具体本机绝对路径也不进仓库文档**。

```powershell
# 前置工具链 PATH（见 §2.2）
$env:Path = "$env:USERPROFILE\.rustup\toolchains\stable-x86_64-pc-windows-msvc\bin;$env:Path"

# 密钥在仓库根的同级 keys 目录 —— 用相对语义拼，别写死盘符
$repoRoot = (Get-Location).Path            # 在仓库根执行
$keyDir   = Join-Path (Split-Path -Parent $repoRoot) "keys"

& ".\scripts\build-signed.ps1" `
    -KeyFile      (Join-Path $keyDir "ai-gateway-updater.key") `
    -PasswordFile (Join-Path $keyDir "ai-gateway-updater.password")

# 仅校验密钥不构建
& ".\scripts\build-signed.ps1" -KeyFile <...> -PasswordFile <...> -CheckOnly
```

**keyid = `7e7ce64b6fc3a36f`**（2026-10-15 独立复算：解析 `tauri.conf.json` 的 `plugins.updater.pubkey` base64 → 取内层载荷第 2..10 字节）。
⚠ **旧口径 `217C0E2B4321D841` 是错的** —— 照它比对会误判成「密钥不配对」。任何文档里出现它都是错的。

其它已踩过的坑：

- `TAURI_SIGNING_PRIVATE_KEY` 的值必须是密钥**内容**，不是路径。传路径会报 `failed to decode base64 secret key: Invalid symbol 58`（路径里的冒号）。读取方式：`(Get-Content -LiteralPath $key -Raw).Trim()`。
- **缺口令时 `tauri signer sign` 不报错，而是阻塞等待 stdin 输入**，表现为「构建卡住」；口令**错误**才会秒级报错。非交互场景务必先 `-CheckOnly`。
- 加密私钥的 keynum 偏移（54..62）与公钥（2..10）**不同，不能直接比对**；判断配对的唯一可靠方式是**实际签一次**再比签名与公钥的 keyid。

---

## 3. 硬约束（不许破坏）

### 3.1 绝不运行安装包 / 卸载器

包括 `AI-Gateway_*_setup.exe` 与 `uninstall.exe`，**加 `/S` 更危险**。

原因：Tauri 的 NSIS 模板按**可执行文件名**匹配并结束进程，**不比对路径**：

```nsis
nsis_tauri_utils::FindProcessCurrentUser "${executableName}"
nsis_tauri_utils::KillProcessCurrentUser "${executableName}"
IfSilent kill_${UniqueID} 0        ; 静默模式下不询问，直接杀
```

`src-tauri/installer.nsi` 实测（2026-10-15）在两处调用：**:651** 与 **:788** `!insertmacro CheckIfAppIsRunning "${MAINBINARYNAME}.exe" "${PRODUCTNAME}"`。
（别按行号找 —— 它会随模板版本漂移，按宏名 `CheckIfAppIsRunning` 搜。）

主程序名 `ai-gateway.exe` 与业主正在运行的实例**同名** ⇒「装一次 / 卸一次」就会杀掉他的实例；而它是 Job Object（`KILL_ON_JOB_CLOSE`）的父进程，父进程一死，网关被系统连带回收 —— **一次误操作打掉两个服务**。

**要验证安装包内容请用解包**（§5.4），不需要安装。

### 3.2 绝不按进程名批量结束进程

```powershell
# ❌ 禁止
Get-Process -Name "gateway*" | Stop-Process

# ✅ 只允许 PID + 路径双条件
$proc = Get-CimInstance Win32_Process -Filter "ProcessId=$pid" -ErrorAction SilentlyContinue
if ($proc -and $proc.ExecutablePath -like "*test-bin*") { Stop-Process -Id $pid -Force }
```

### 3.3 绝不直接运行共享目录里的可执行文件（**哪怕只是 `--version`**）

**2026-10-01 事故**：队友为「取一下版本号」跑了 `& "crates\ai-gateway-core\embedded\gateway.exe" --version`。
他以为「取版本号」不算运行 —— **它就是运行**。

**为什么危险**：

1. **Windows 对正在运行的可执行文件加锁** ⇒ 此时任何 `cargo build` 都报 `failed to remove file ... gateway.exe` / `拒绝访问。(os error 5)`。本仓库 2026-10-01 就因此**整次打包失败**（`cargo build exit 101`）。
2. **它可能不只打个版本号就退出**。**2026-10-15 读源码确认**：网关 `crates/ai-gateway-router/src/bin/gateway.rs` 的 `main()` **没有 `--version` 分支** —— 子命令派发只认 `qoder-login` / `zcode-login`（`dispatch_subcommand`，:1174），其余参数落到「无 `-config` 启动」⇒ **照常起 HTTP 服务（默认 `:7863`）并常驻**。
3. 若该二进制恰好与业主在跑的实例同名，还会踩到与 §3.2 同型的坑。

⚠ **判据不是「你打算干什么」，而是「你有没有碰那个共享文件」** ——「探一下」「看一眼」「取个字符串」同样适用。

> ★ **一个精确区分（2026-10-15 核实）**：**宿主服务端** `crates/ai-gateway-server` 的 `main.rs:112` **确实有** `version | --version | -V` 分支。
> 即「`--version` 会起服务」这条**只对网关（`gateway.exe`）成立**，对 server 不成立。**但规矩不变**：两个都别直接跑共享目录里的那份。

```powershell
# ✅ 要检查任何共享目录里的可执行文件，先拷出来
Copy-Item "crates\ai-gateway-core\embedded\gateway.exe" "$env:TEMP\probe-gw.exe"
& "$env:TEMP\probe-gw.exe" --version
```

### 3.4 验证一律用独立实例：独立名 + 独立端口 + 独立数据目录

- **独立数据目录**：`$env:AI_GATEWAY_HOME`（`crates/ai-gateway-core/src/modules/config.rs` 认这个变量，专门用于开发/测试隔离）。
  ⚠ **不设它就会用真实数据目录，还会触发网关重启** —— 已有把业主 `:7864` 的 pid 从 24900 换成 42576 的事故记录。
- **独立端口**：避开 `:7864`（业主网关）/ `:3080`（DSH Desktop）/ `:57890`（server 的 `default_port()`，见 `crates/ai-gateway-server/src/main.rs`）。实测惯例用 `5789x` / `5799x`。
- **独立名**：产物改名，确保与业主的进程都不同名。
- 收尾只按 **PID + ExecutablePath 双条件**清理（§3.2），并确认目录里**只有日志与 exe**。

```powershell
$home1 = Join-Path $env:TEMP "inst-<用途>"     # 别写死盘符
New-Item -ItemType Directory -Force -Path $home1 | Out-Null
$env:AI_GATEWAY_HOME = $home1
$p = Start-Process -FilePath "<独立名>.exe" -ArgumentList "serve","--port","57899","--no-open" `
     -PassThru -WindowStyle Hidden `
     -RedirectStandardOutput "$home1\o.log" -RedirectStandardError "$home1\e.log"
"PID=$($p.Id)" | Out-File "$home1\pid.txt" -Encoding ascii
```

### 3.5 ⚠ 两个 crate 的产物同名，会互相覆盖

`crates/ai-gateway-server`（`[[bin]] name = "ai-gateway"`，约 15MB）与 `src-tauri`（`[package] name = "ai-gateway"`，约 30MB）**输出到同一个 `target\release\ai-gateway.exe`**。

拿它做接口测试可能拿到 **GUI** —— GUI 带单实例插件，发现业主的实例在跑就**自己静默退出**（无输出、不监听），极易误判成「代码启动即退出」。

```powershell
# ✅ 用独立 target 目录构建 server 版，避免覆盖 GUI 产物
cargo build --release -p ai-gateway-server --target-dir <独立目录>
# 产物约 15MB = server 版；30MB 就是 GUI，别用
```

> ⚠ 若设了 `CARGO_TARGET_DIR`，`.gitignore` 只忽略 `target/` 与 `target-*/`；**别的自定义目录名会漏进 `git status`**。

### 3.6 业主的服务必须保持在线

`:7864`（网关）与 `:3080`（DSH GUI）**必须全程在听**。每次验证**前后**都自检：

```powershell
foreach ($port in @(7864, 3080)) {
    $c = Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue
    if (-not $c) { Write-Warning "所有者的 :$port 未在运行 —— 立即停止并报告" }
}
```

⚠ **端口清单以现场为准，不要背数字**。**2026-10-15 实测**：`:7864` 在听
（PID 13884 = `gateway-509bdb-*.exe`）、`:3080` 在听（PID 38744 = `node.exe`，DSH GUI）、
`:43120` **未监听**。

> ⚠ **同日更正**：根 `AGENTS.md` 此前写 DSH 在 `:43120`，照它做自检会**误报「业主服务掉了」**。
> 根 `AGENTS.md`、`README.md`、本页、[RUNBOOK.md](RUNBOOK.md)、[TESTING.md](TESTING.md)
> 已**全部统一为 `:7864` / `:3080`**（2026-10-15）。
> 判据是**现场实测**（`Get-NetTCPConnection` / `netstat -ano`），不是任何文档里的数字 ——
> 端口会变，背下来的清单迟早会骗你。

### 3.7 公开仓库：不得写入真实账号标识

本仓库是**公开仓库**。排查线上问题时很容易顺手把真实值粘进注释或测试夹具 —— 历史上在 `cred.go` 注释、`device_mid_*_test.go` 夹具、`usage.go` / `identity.go` 示例、Rust doc 注释里一度散落着**真实账号 UUID / deviceMid / 上游账号 ID**。

占位值用**一眼能看出是假的**形式：`{uuid}`、`12345678901234567`、`00000000-0000-4000-8000-000000000001`。

⚠ **改完必须重跑测试**：占位值若参与断言（如 UUID 形态校验），换成非法形态会让测试失败 —— 那正是它该失败的时候。

### 3.8 `.ps1` 必须带 UTF-8 BOM

**症状**：脚本突然报一堆位置毫无道理的语法错（`Missing expression after ','` / `Unexpected token '}'` / `The string is missing the terminator`）。

**根因**：脚本里有大量中文注释，而 **PowerShell 5.1 在没有 BOM 时按 ANSI（本机 GBK）读取 `.ps1`** ⇒ 中文全乱码，引号与花括号配对崩掉。

**2026-10-15 实测**：`scripts/build-signed.ps1` / `build-gateway.ps1` / `build-single.ps1` **三者都带 BOM**（首三字节 `239,187,191`）—— **保持住**。

**触发方式（已踩两次）**：① `[System.IO.File]::WriteAllText($p,$t,UTF8Encoding($false))` —— `$false` 表示**不要 BOM**；② 用 `edit` 工具改这个文件 —— **它也会去掉 BOM**。

**改脚本的规矩**：改完**必须补 BOM**，并**必须**用 `Parser::ParseFile` 验证（它处理 BOM，`ParseInput` 不行）：

```powershell
$err = $null
[System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$null, [ref]$err) | Out-Null
if ($err.Count) { $err | Select-Object -First 3 | ForEach-Object { "行 $($_.Extent.StartLineNumber): $($_.Message)" } }
```

⚠ **别用 `git checkout HEAD -- <脚本>` 回退** —— 那会把你**未提交**的改动一起丢掉（曾因此丢掉「构建内嵌网关」整段逻辑）。误删后可从悬空对象找回（`git fsck --lost-found` → `git cat-file -p <blob>`）。

---

## 4. 工作流

1. **先读文档**：本页 → [README.md](README.md) 文档地图 → 与改动相关的主题文档。
   查「为什么当初这么选」翻 **`.agents/notes/`**（决策笔记，按 `{lifecycle}/{class}`
   分目录；原 `docs/DECISIONS.md` 的 D01–D20 已全部迁入该树，原文件已删除）。
2. **改代码**：改动范围只落在被指派的部分；连带影响可自主修掉，**不相关的别碰**。
3. **跑门禁**：`npm run build` + `cargo test --workspace`，**不低于 §5.1 的基线**。
4. **同步文档**：行为变化 → [FEATURES.md](FEATURES.md)；**决策变化 → `.agents/notes/`**；流程变化 → [RUNBOOK.md](RUNBOOK.md)；测试体系变化 → [TESTING.md](TESTING.md)。文档与代码**同一次提交**更新。
5. **交付前必须自己跑实测**（§5.2）—— 这是硬要求，不是建议。
6. **提交 / 推送**：本地 commit 允许且鼓励；**push 必须等业主显式授权**。

### 4.1 上游协议细节：先读抓包规格

改动上游协议细节前，先读 `go-gateway/internal/zcode/CAPTURED-SPEC.md`（由抓包到的**成功请求**生成）。它开头写着：**这是唯一可信的规格来源 —— 此前所有关于该通道的结论都是推断，且多次出错**。

⚠ **参考实现的注释是二手信息**。曾有一条注释「start-plan 只发 3 个头，误发 `x-query-id` / `x-session-id` 会触发 3012」看起来完美解释问题，据此改代码后核对抓包发现**官方客户端确实发了这两个头** —— **那条注释是错的**，改动全部撤销。

---

## 5. 交付与验证纪律

### 5.1 测试基线（改动后不得低于）

⚠ 本表以**版本更晚的记录为准**；任何与现读 `cargo test` 输出不一致的数字，以现读为准：

| 目标 | 最新已知 | 口径来源 |
|---|---|---|
| `cargo test --workspace` | **3081 passed / 0 failed / 7 ignored**（15 suite） | 1.0.36（2026-10-08/09） |
| `cargo test --workspace` → router | **1798 passed / 0 failed / 1 ignored** | 2026-10-16 本轮（版本仍 1.0.46，未 bump；1.0.46 为 1780，净增 18 = 会话头族 5 + 国际版身份头 4 + 熔断区域隔离 3 + 会话头族解析 1 + **11140 误判回归 5**（见 `.agents/notes/implemented/bug-fix/2026-10-16-banned-11140-not-3012.md`）；另新增线级套件 `tests/ab_conversation_headers.rs` 3 条，**不在 lib 计数内**） |
| `cargo test --workspace` → core | **1301 passed / 0 failed / 4 ignored** | 1.0.46（2026-10-14，§9.21） |
| `cargo test --workspace` → src-tauri | **28 passed / 0 failed / 0 ignored** | 2026-10-16 本轮（1.0.46 基线为 17；新增 7 条设备 token 发现门禁 + 4 条既有） |
| `cargo test -p ai-gateway-router` | 1685 / 0 / 1 | **1.0.34 旧口径**，已被 1780 取代 |
| `cargo check -p ai-gateway-router --lib` / `--bin gateway` | EXIT 0，**0 warnings** | 1.0.34 |
| `cargo check --workspace --all-targets --profile test` | EXIT 0 | 与 CI 同款 |

★ **1.0.46 的 workspace 总计数没有单独记录**（§9.21 只记了 core / router 两个分项）—— **未证，别编**。
★ 权威口径以 `crates/ai-gateway-router/Cargo.toml` 现读的 `version` 为准，**别在测试/脚本里写死版本号**（曾因此产生假红，已改为现读）。

### 5.2 ★★ 「实测」的定义（三者缺一不可）

1. **真实启动**一个**新构建**的实例（独立端口 + 独立数据目录，见 §3.4）
2. **真实发请求**走完整链路（`curl` 打到 `/v1/chat/completions`，不是只跑单测）
3. 看到**期望的输出**（200 + 正确内容；或明确的、可解释的错误）

**以下都「不算实测」，不得据此交付**：

- 编译通过 / `cargo check` / `go build`
- 单元测试全绿（它们证明不了「打包后能跑」）
- 产物 SHA256 与源码比对一致
- 「我在自己的实例上看到 200」—— 除非那是**当次构建**的产物

业主原话（2026-09-22）：**「每次完成需求或者修复bug之后,必须强制跑实测,才能交付给我」**。

### 5.3 ⚠⚠ `curl` 的引号陷阱（看起来像网关 bug，实际是测试方法错了）

PowerShell 用**单引号**包 JSON 会**吃掉内部双引号** ⇒ 请求体变成 `{model:xxx,...}`（非法 JSON），上游报
`11101 invalid character 'm'`。**一律用文件传 body**：

```powershell
$json = '{"model":"qoder:Qwen3.8-Flash","messages":[{"role":"user","content":"hi"}],"stream":false}'
[System.IO.File]::WriteAllText("$tmp\r.json", $json, (New-Object System.Text.UTF8Encoding($false)))
curl.exe -s -X POST "http://127.0.0.1:$port/v1/chat/completions" `
  -H "Authorization: Bearer sk-123" -H "Content-Type: application/json" `
  --data-binary "@$tmp\r.json" --max-time 60 -w "`n[HTTP %{http_code}]"
```

⚠ **判定「功能是否真的生效」必须绕过兜底路径**。例：ZCode 验证码的实现是「外部求解失败 → 回退本地 Node」⇒ **发对话请求不能作为判据**（外部全坏时，只要本机装了 Node，对话照样成功）。要直接调宿主求解服务那条**没有兜底**的路。

### 5.4 交付前自检：解包比对（**无需安装**）

**内嵌网关**：内嵌的网关是主程序 exe 内的一个 **gzip 流**。扫描 gzip 魔数 `1F 8B 08` → 解压 → 应以 `4D 5A`（MZ）开头且 **> 1MB** → 与 `crates\ai-gateway-core\embedded\gateway.exe.sha256` 比对。

**内嵌前端**：只能用**资源文件名**判断，**绝不能用「在 exe 里搜业务字符串」**：

```powershell
# dist/index.html 写死了它引用哪个 assets/index-XXXX.js
$want = [regex]::Match((Get-Content dist\index.html -Raw), 'assets/index-[A-Za-z0-9_\-]+\.js').Value
# 那个名字是 Vite 按**内容哈希**生成的，且文件名在 exe 里是**明文**
$got = [regex]::Matches([System.Text.Encoding]::ASCII.GetString(
    [System.IO.File]::ReadAllBytes('target/release/ai-gateway.exe')), 'assets/index-[A-Za-z0-9_\-]+\.js') |
    ForEach-Object { $_.Value } | Sort-Object -Unique
$got -contains $want    # True = 内嵌的是当前前端
```

**为什么不能用搜字符串**：Tauri 对 JS **内容**做了压缩/编码，`product-usage-bucket` 这类标识在 exe 里**搜不到** —— 连**早就存在**的老标识也搜不到。**搜不到 ≠ 没打包**。曾据此得出错误结论，白绕一大圈。

**背景（为什么需要这两道闸）**：`crates/ai-gateway-core/build.rs` 会**无条件内嵌** `embedded/gateway.exe` 里当时躺着的那一份；`tauri-build` 的 `rerun-if-changed` **不包含** `../dist`。⇒ 改了网关 / 前端却**不重建**，打出来的包内嵌旧版本，而**构建日志没有任何异常**。两者是**同型缺陷**。`build-signed.ps1` 已固化这两道校验（`==> 校验内嵌网关指纹` / `==> 校验内嵌前端指纹`）。

**手工构建时必须自己补这两道校验**。

### 5.5 CI（`.github/workflows/ci.yml`）—— 三个 job，各有非显然理由

| job | 跑什么 | 为什么单独存在 |
|---|---|---|
| `check` | `ubuntu-22.04` / `macos-14` / `windows-latest` 三平台：`npm run build` → `cargo test -p ai-gateway-core` → `cargo check --workspace --all-targets --profile test` → macOS 另加 `--target x86_64-apple-darwin` | 历史上多次出现「只在某一个平台编译失败」的回归 |
| `gateway` | `cargo test -p ai-gateway-router --lib` → `cargo test -p ai-gateway-server` → `sh scripts/build-gateway.sh` | 见下四条 |
| `release-bin` | `cargo build -p ai-gateway-server --release` | release profile 与 check/test profile **产物不通用**，且只有 release 才暴露 LTO / 优化期才炸的错误；`release.yml` 里虽也构建，但那是 **tag 之后**，那时发现已经晚了 |

**四条必须知道的坑**：

1. **`--profile test` 不可省**：`cargo test` 默认在 `test` profile 构建（继承 dev，但 `debug-assertions` / `overflow-checks` 与 dev 不同 ⇒ 产物不通用），而 `cargo check` 默认用 `dev`。不加会**在同一个 job 里完整编译两遍**（Windows 实测第二遍 19–21s，而 `windows-latest` 正是关键路径）。
2. **`cargo test -p ai-gateway-server` 必须显式加**：那里住着一条「前端 ROUTES ↔ 服务端路由」一致性测试（`api.rs::every_frontend_route_path_has_a_server_handler`），专门抓「前端登记了路由、服务端没实现」这个**反复出现**的缺陷类。**不加这条，那个测试永远不会被执行 —— 写了等于没写**（2026-10-01 据此查出并修好 4 条真实缺口）。
3. **必须先 `npm run build`**：`ai-gateway-server` 用 `RustEmbed` 嵌 `../../dist/`，而 `dist/` 在 `.gitignore` 里 ⇒ 刚 checkout 出来**根本不存在**，`#[derive(RustEmbed)]` 直接报 `folder '…/../../dist/' does not exist`（并连带两条 E0599）。`gateway` 与 `release-bin` 两个 job 都受此约束。
4. **CI 走的是真正的构建入口 `sh scripts/build-gateway.sh`**，而不是 `cargo build -p … --bin gateway` —— 这样 CI 验证的是**打包时实际执行的那条路径**。历史上「脚本能跑但 CI 用的是另一条命令」正是回归能溜过去的原因。

### 5.6 证据与产物都不入库

- `uitest/`（实测证据日志）、`test-bin/`（隔离实例 + `DELIVERY-*.md` 交付说明 + 侦察报告）、`dist-single/`、`dist-bin/` 都在 `.gitignore` 里。
  **理由**：① 体积（每个隔离实例都有一份网关副本，12MB 量级）；② **泄漏面**（侦察报告与探针脚本可能夹带真实账号标识）。
- ⚠ **`uitest/` 当前只有 22 个 `.log` 证据日志**（2026-10-15 实测）。
  **根 `AGENTS.md` 引用的 `uitest/page-harness.cjs` / `uitest/mock-host-api.cjs` / `uitest/README-验证约定.md` 在磁盘上均不存在**（全仓库查找无匹配；根 `AGENTS.md` 的 2026-10-15 更正另记载它们**从未入库**，且 `uitest/` 是未跟踪目录）。
  ⇒ 想按「前端/UI 验证不需要起后端」那条路走，**需先重建这些探针**；在它们存在之前，那些步骤**不可执行**。


---

## 6. ★★ 本仓库特有的陷阱（改代码前务必读）

### 6.1 `cargo test --workspace` 与 `-p` **不是同一个构建**

`src-tauri` 的 `tauri-plugin-updater` 拉 reqwest 0.13.4，打开 `hyper-util/client-proxy-system`。`hyper-util` 全 workspace 只有一个版本 ⇒ **Cargo 按包版本统一 feature** ⇒ 我们自己的 reqwest 0.12.28 在 `--workspace` 构建里**也**拿到系统代理能力，`-p` 构建里没有。

后果：reqwest 默认 `auto_sys_proxy=true`，会读 **Windows 注册表**的系统代理（Go 的 `http.ProxyFromEnvironment` 只读环境变量）⇒「没配代理」在 Go 是真直连、在 reqwest 是偷偷走代理。

**修法**：未配置代理时必须显式 `.no_proxy()`（`upstream/client.rs` 的 `match proxy`；2026-10-15 核实 :154 与 :287 两处）。**有测试钉住这条。**

### 6.2 ★★ `Copy-Item` 保留 mtime ⇒ cargo 跳过重建 ⇒ 测到旧二进制（**假绿**）

做 A/B 还原验证（「还原修复看测试是否变红」）时**必须**：

```powershell
(Get-Item <file>).LastWriteTime = Get-Date
```

否则「还原后测试仍通过」是假象 —— 它跑的是上一轮的二进制。**这个坑踩过两次。**

⚠ 而且**要确认改到了真正生效的分支**：曾还原函数**末尾**那行，而该场景下代码在**前面**就 return 了 ⇒ 改的那行**根本没被执行** ⇒ 测试「通过」。可靠做法是在还原处加一行 `println` 看它是否真的被打印。

### 6.3 `serde_json` 无 `preserve_order` ⇒ `Map` 是 `BTreeMap`

`json!({...})` 会按键的**字典序**重排字段。凡「字段顺序参与签名」的上游请求体，**必须**用 `#[derive(Serialize)] struct` 按声明序序列化。这类错误**不报错**，只让上游签名校验失败。

### 6.4 `bin/gateway.rs` 是**独立 crate**

看不到 lib 的 `pub(crate)` 项。需要 `pub(crate)` 依赖的逻辑**必须**在 lib 内做成 `pub` 入口，bin 只调用它。

### 6.5 文本工具的几个假象（本仓库的 `.rs` 是 CRLF）

- ★★ **审计「定义 vs 注册」清单时别用 `-Context 0,N`** —— 它会在 N 行处**静默截断**，长清单被少算（曾据此误报「10 个 Tauri 命令未注册」，实际 86=86 零缺口）。正解：`ReadAllLines()` 定位块起止行，在**完整行区间**上迭代正则。
- ★★ **本仓库的 `.rs` 是 CRLF**（`api.rs` 实测 2943 CRLF / 0 裸 LF）⇒ 跨行匹配必须先 `replace("\r\n","\n")`，否则含 `\n` 的模式**永远匹配不上**（假红）。
- `Get-Content` 把 UTF-8-no-BOM 的 `.rs` 读成 **GBK** ⇒ 用 `read` 工具；数行数用 `[System.IO.File]::ReadAllLines($p).Length`。
- `Select-String` 单引号里的 `\"` 是**字面量** ⇒ 会假阴性。
- `cd D:\...` **不改变 .NET 进程 CWD** ⇒ `[System.IO.File]` 必须用**绝对路径**。

### 6.6 测试装置本身会伪装成「产品缺陷」

- **假上游必须用 `ThreadingHTTPServer` + `daemon_threads`**；单线程会让网关**死锁**。
- **真机断言的期望值一律从真机数据推，不许写死**。曾把 `trae-checkin` 期望写死成 `done` —— 而它只在「今天已签过到」时是 `done`，跨过午夜或换个今天没签到的实例**必然变红**，产品渲染完全正确。同理版本号期望必须现读（§5.1）。
- **数据读不到 / 同一平台多账号推导不一致 ⇒ 返回 `null` ⇒ 不断言**（宁可少一条，也不产生假 FAIL）；并且**惰性求值**（断言那一刻才读盘），避免宿主后台改写造成假红。
- **CDP 注入落进 `about:blank` 上下文（竞态）**：WebView2 先加载 `about:blank`，约 700ms 后才导航 ⇒ `Runtime.evaluate` 落在**当时**的上下文，导航触发 `executionContextsCleared` 把注入的变量一起丢弃 ⇒ 表现为「一整排断言全红、全是 helper undefined」。
  修法：① 注入前 `Page.addScriptToEvaluateOnNewDocument`；② 等上下文稳定（`location.href` 含 `tauri.localhost` 且连续两次相同，上限 30s）；③ 注入后带重试；④ **必须为「注入本身」加一条断言**（否则注入失败会被后续断言掩盖成产品缺陷）。
- **卡死类缺陷的可复现判别式 = CDP 事件流里的 `Target.targetCrashed` 计数**，**不是**「响应/不响应」（后者取决于崩溃时刻是否撞上探针超时线，同一装置可能首轮命中、后两轮不命中）。

### 6.7 几个「看着像 bug、其实是刻意设计」的地方

- `gateway_embed` 的 FNV-1a 用的是**写错一位的质数** `0x1000_0000_01b3`（标准是 `0x100000001b3`）。**只影响落地缓存文件名**，仍稳定且对内容敏感 ⇒ **不修**（改质数会让所有用户的落地文件名变化、触发一次无谓重写）。
  ★ 落地文件名里的**长度段是 gzip 载荷长度，不是解压后的 exe 长度** —— 写断言脚本时**照源码口径复算**，别自作主张用解压后长度（曾因此吃假红）。
- **Rust release 构建不确定**：同一份源码两次构建 SHA256 **不同** ⇒ 网关指纹**只在同一次构建链内可比**，跨次构建不能按哈希比对。
- **`ai-gateway-server` 不在 NSIS 安装包里** —— 它经 **npm** 分发。改它**不需要**重打 `dist\*.exe`。
- **`axum` 在客户端断开时会丢弃 handler future** ⇒ 清理逻辑必须用 **RAII 守卫**。
- `[profile.release]` **刻意不设** `lto="fat"` / `codegen-units=1`：会让每次 release 构建的单核链接时间显著拉长，而本仓库 release 构建非常频繁。需要极致产物时用 `CARGO_PROFILE_RELEASE_LTO=fat` 临时覆盖。

### 6.8 跨模块时序陷阱（改选号 / 模型清单前必读）

- **「宿主写配置」与「网关读配置」的时序**：`product_models`（网关判断「哪个产品声明提供该模型」的清单）在 `start_gateway` 写配置时算 —— 而那一刻 qoder / zcode 账号库里的 `models` **还是空的**（要靠启动 90 秒后的巡检去上游查）。
  ⇒ **任何改变 `product_models` 输入的操作（刷新账号、导入账号、账号巡检）都必须跟着调一次 `gateway::resync_native_config()`**（2026-10-15 核实该函数存在，且在 `multi_product_credit_patrol::run_once` 里被调到）。
  它**只写文件、不重启网关**（重启会掐断正在进行的对话）⇒ 清单对**已在运行**的网关要等下次重启才生效。**这个取舍要写进给用户的说明里**，别让用户以为「装完立刻生效」。
- **「兜底清单」不能参与「声明」竞争**：清单分两份，语义严格区分 ——
  `productModelSet`（宿主透传，上游**真实查询**）参与「不排除」**+**「声明」；`productModelFallbackSet`（网关内置静态表，**猜测**）**只**参与「不排除」。
  即「不知道某产品提供什么 ⇒ **别排除**它，但也**别声称**它提供」。加任何「补全清单」的逻辑前先问：**它会被当成声明吗？** —— **猜测永远不该赢过事实**。
- **熔断不是「停服」**：单账号产品（qoder / zcode）必须退到**同产品内部**的冷却兜底，否则一次上游瞬时故障被放大成 30 分钟停服。
  ⚠ **兜底绝不能跨产品** —— 产品前缀是用户的显式指令，跨产品会拿错凭证、打到错的端点。
- **网关重启必须走 `gateway::restart_gateway_serialized()`**，不要自己写 `stop_gateway()` + `start_gateway()`（多处调用方交错时后到的必然撞上「网关已在运行」检查 ⇒ **实际没重启**）。它用 `tokio::sync::Mutex`（临界区含 `.await`，`std` 的锁跨 await 会阻塞 runtime 且编译不过）。有测试钉住「不得新增绕过锁的裸 `stop_gateway()`」。
- **并发上限与客户端重试**：`max_in_flight` 是**每账号**上限（当前值 **3**，`config.rs` 默认值实测确认），单账号产品上它**就是**每产品上限。而 DSH 用的 `@earendil-works/pi-ai` 默认**并发重试 5 次** ⇒ 上限太小会让客户端**每次都失败**而 `curl` **每次都成功**。
  ★ 加这类魔数**必须写注释说明取值依据**，否则后人只看到 `3`，既不知道从哪来也不敢改。

---

## 7. 不要做的事

- **不要为了「看起来更完整」而补文档章节** —— 只写读不出来的。
- **不要造文档与代码的分叉**：魔数、路径、行号变了就同步改文档 —— **文档与代码分叉比没有文档更糟**。
- **不要跳过实测**：编译过 / 单测绿 / SHA256 一致，都**不算**实测（§5.2）。
- **不要把真实账号标识、真实密钥、口令写进任何被 git 跟踪的文件**（§3.7）。
- **不要把本机绝对路径写进仓库文档** —— 判据是「别人 clone 到就成立」；只对本机成立的（本机路径、探针产物）不进仓库。
- **不要运行安装包 / 卸载器，不要按进程名杀进程，不要直接运行共享目录里的可执行文件**（§3.1–3.3）。
- **不要在没有授权时 push** —— 本地 commit 不限量，**推远端需业主显式授权**（历史授权范围只覆盖 `fork`，其它 remote 不许碰）。
- **不要用 `git checkout HEAD -- <脚本>` 回退脚本**（会丢掉未提交改动，§3.8）。
- **不要执行 `git add -A` 式的大范围操作**，也别把探针产物 / 隔离实例带进仓库。
- **不要直接 `tauri build` 就交付** —— 必须走 `build-signed.ps1`（它会重建内嵌网关并做双道指纹校验，§5.4）。

---

## 8. 去哪找更多

| 我想…… | 翻这份 |
|---|---|
| 看 `docs/` 全部文档与分工 | [README.md](README.md) |
| 知道「为什么当初这么选」/ 想推翻旧结论 | **`.agents/notes/`**（决策笔记，唯一规则；原 DECISIONS.md 的 D01–D20 已全部迁入该树） |
| 发版、本机跑起来、按现象排障、回滚 | [RUNBOOK.md](RUNBOOK.md) |
| 写测试 / 跑验收 / 怀疑门禁是假的 | [TESTING.md](TESTING.md) |
| 改用户能看见的行为之前对齐语义 | [FEATURES.md](FEATURES.md) |
| 纯部署版（server 单文件）怎么发 | [SERVER-DEPLOY-1.0.46.md](SERVER-DEPLOY-1.0.46.md) |
| 查「实现搬过来了、接线丢了」这类 P0 | [GO-STARTUP-WIRING-AUDIT.md](GO-STARTUP-WIRING-AUDIT.md)（**历史审计，只读**） |
| 查硬约束（Shell / 组件 / 构建签名 / 验证纪律） | [../AGENTS.md](../AGENTS.md) —— 仓库根，与本页**并存** |
| 看用户视角的功能导览与安装说明 | [../README.md](../README.md) |
