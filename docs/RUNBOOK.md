# 运维与发版手册（RUNBOOK）

> 定位：**发版 / 本机跑起来 / 按现象排障 / 回滚 / 卸载**的操作手册。
> 只写代码里读不出来的东西（为什么、约束、陷阱、验收标准、非显然步骤）。
> 最后核对：**2026-10-15** ｜ 对应分支 `main` ｜ 版本 **1.0.46** ｜ 核对时 HEAD `dc729e9`
>
> 命令一律 **PowerShell**（仓库强制，见 [../AGENTS.md](../AGENTS.md) 的 Shell Policy）。
> 文中 `%USERPROFILE%` 表示当前用户主目录，实际写命令时用 `$env:USERPROFILE`。

## 0. 读之前：三条会伤到人的红线

| # | 红线 | 为什么 |
|---|---|---|
| 1 | **绝不运行安装包 / 卸载器**（`AI Gateway_*_setup.exe`、`uninstall.exe`），**加 `/S` 更危险** | Tauri 的 NSIS 模板按**可执行文件名**匹配并结束进程，不比对路径；主程序名 `ai-gateway.exe` 与所有者正在跑的实例**同名**，而它是 Job Object（`KILL_ON_JOB_CLOSE`）的父进程 ⇒ 一次误操作打掉两个服务 |
| 2 | **绝不按进程名批量杀进程** | 只允许 **PID + `ExecutablePath` 双条件**（见 §3.4） |
| 3 | **绝不直接运行共享目录里的可执行文件，哪怕只是 `--version`** | Windows 对运行中的 exe 加锁 ⇒ 后续 `cargo build` 报 `拒绝访问 (os error 5)`；且网关 `main()` **没有 `--version` 分支**，参数会被当成「无 config 启动」⇒ 起 HTTP 服务并**常驻**。要读版本先 `Copy-Item` 到临时目录再执行 |

> 这三条不是「注意一下」，是**已经发生过**的事故（2026-09-16 / 2026-10-01）。完整记录见 [../AGENTS.md](../AGENTS.md) 的「验证与测试」节。

---

## 1. 发布一个版本

### 1.1 版本号：一条命令改 **14 个文件**

```powershell
cd <仓库>\wt-port
& .\scripts\bump-version.ps1 1.0.47
```

**必须用脚本改，不要手改。** 版本号散在 14 个文件里，漏一处就有具体后果（不是"不一致"这么抽象）：

| 文件 | 漏改的后果 |
|---|---|
| `package.json` / `package-lock.json` / `npm/package.json` | npm 包版本与安装包不一致 |
| `src-tauri/tauri.conf.json` | **安装包版本号不对** ⇒ NSIS 同版本不覆盖（见 §1.5），升级会走进"维护页"而不是升级 |
| `src-tauri/Cargo.toml` | `ai-gateway.exe version` 输出错版本（**唯一可信的版本来源**，见 [SERVER-DEPLOY-1.0.46.md](SERVER-DEPLOY-1.0.46.md) §1.2） |
| `crates/ai-gateway-{core,router,server}/Cargo.toml` | 各 crate 版本分裂 |
| `npm/platform/*/package.json`（**5 个**） | `optionalDependencies` 指向不存在的版本 |
| `Cargo.lock` | `cargo build --locked` 失败 |
| `package-lock.json` | lockfile 与 manifest 版本漂移 |

**脚本内已经修掉的两个历史缺陷**（别"优化"回去）：

- `Cargo.lock` 的替换**锚定包名**，显式包含根包 `ai-gateway`（旧 `bump-version.sh` 的正则匹配不到它，根包会留在旧值）。
- `package-lock.json` 必须锚定 `"name": "ai-gateway"` —— 该文件里有**约 270 处** `"version": "x.y.z"`，用全局 pattern 会把所有依赖的版本号一起改掉，**直接毁掉 lockfile**。

**两条硬约束**：

1. 版本号必须是 `x.y.z` 三段纯数字 —— 脚本用 `^\d+\.\d+\.\d+$` 校验，**预发布后缀会被直接拒绝**（预发布要走 tag 名，见 §1.4）。
2. 脚本会把**进程级 cwd**（`[Environment]::CurrentDirectory`）也设到仓库根。它下面大量用 `[System.IO.File]::ReadAllText("package.json")` 这类**相对路径**，而 .NET 静态 API 解析相对路径用的是进程 cwd、不是 PowerShell 的 `$PWD`；`Set-Location` 只改后者。**从仓库根以外的目录调用是支持的**，不要"帮它"去掉那一行。

### 1.2 本地签名构建：`scripts/build-signed.ps1`

**不要直接跑 `npm run tauri -- build`。** `tauri.conf.json` 的 `createUpdaterArtifacts` 一直是 `true`，构建**必须**拿到 minisign 私钥；而这个脚本包干了三件直接跑 tauri 不会做的事：

1. 构建前做**密钥/口令预检**（缺口令时 tauri **不报错而是阻塞等 stdin**，见 §2.3）；
2. 第 `[3/4]` 步**强制重建内嵌网关**；
3. 构建后做**内嵌网关 + 内嵌前端双指纹校验**，并把产物复制到固定交付目录。

**完整命令**（本机密钥不在脚本默认位置，故必须显式传参，原因见 §2.1）：

```powershell
cd <仓库>\wt-port

# ① cargo 不一定在 PATH 里（这一步最容易卡住，见 §1.3）
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

# ② 定位密钥目录（★ 在仓库根**之外**、仓库根的**同级** —— 别写死盘符/用户名）
$repoRoot = (Get-Location).Path                          # 本行必须在 wt-port 仓库根执行
$keyDir   = Join-Path (Split-Path -Parent $repoRoot) "keys"
Test-Path (Join-Path $keyDir "ai-gateway-updater.key")   # 必须先为 True

# ③ 先只做密钥预检（秒级，绝不浪费一次完整构建）
& .\scripts\build-signed.ps1 `
  -KeyFile      (Join-Path $keyDir "ai-gateway-updater.key") `
  -PasswordFile (Join-Path $keyDir "ai-gateway-updater.password") `
  -CheckOnly

# ④ 预检通过后再正式构建
& .\scripts\build-signed.ps1 `
  -KeyFile      (Join-Path $keyDir "ai-gateway-updater.key") `
  -PasswordFile (Join-Path $keyDir "ai-gateway-updater.password")
```

> ⚠ **别用 `pwsh`**：本机（2026-10-15 实测）**没有装 PowerShell 7** —— `Get-Command pwsh` 找不到，文档里写的 `pwsh scripts/...` 会直接报「无法将 pwsh 项识别为 cmdlet」。
> 用上面这种 `& .\scripts\<脚本>.ps1` 的形式（在已有 PowerShell 会话里执行），或 `powershell.exe -NoProfile -File <脚本>`。**别据此判定脚本坏了。**

**参数（逐字核对源码 `scripts/build-signed.ps1:48-59`）**：

| 参数 | 默认 | 说明 |
|---|---|---|
| `-Bundles` | `"nsis"` | 要构建的 bundle 类型，多平台用**逗号分隔**，如 `"nsis,msi"` |
| `-KeyFile` | 空 ⇒ 走 `Resolve-KeyPath` | 私钥路径 |
| `-PasswordFile` | 空 ⇒ 走 `Resolve-KeyPath` | 口令文件路径 |
| `-CheckOnly` | 关 | **只做密钥预检，不构建** |
| `-SkipGateway` | 关 | 跳过网关重建，沿用 `embedded/` 现有产物 |

**四个步骤（不是三个）**，日志前缀就是判据：

```
==> [1/4] 检查签名私钥
==> [2/4] 校验口令与密钥配对
==> [3/4] 构建/刷新内嵌网关（Rust）
==> [4/4] 构建签名安装包（bundles=...）
==> 校验内嵌网关指纹
==> 校验内嵌前端指纹
==> 校验产物签名
==> 复制产物到 dist/
```

> ⚠ **`-SkipGateway` 放弃的是"内嵌网关新鲜度"这个保证**，不只是省 10 秒。只在你**确定网关源码没动、只想重打前端/UI** 时用。它仍会跑指纹校验，但基准是**旧产物** —— 校验通过不代表包里的网关是新的。

> ⚠ **两个脚本的 `-CheckOnly` 不是一回事**：`build-signed.ps1 -CheckOnly` 只查密钥；`build-gateway.ps1 -CheckOnly` 只查网关产物新鲜度。别混用。

**产物落点（两个，都要知道）**：

| 目录 | 内容 | 性质 |
|---|---|---|
| `target\release\bundle\nsis\` | 构建原生产物 + `.sig` | 中间位置，会被 `cargo clean` 清掉 |
| `..\dist\`（= 仓库根的**同级**目录） | 安装包 + `.sig` | **固定交付目录**，脚本自动复制并复核 SHA256 |

> ⚠ 脚本对 `dist/` **只复制不清理**（那里还躺着历史草稿与 md）。同时它是**复制它找到的每一个 `.sig`** —— 而 `target\release\bundle\` 会累积历史版本（本机核对时该目录下仍躺着 1.0.9 系列）。**构建前先清掉 bundle 目录**，否则旧包会被一起复制进 `dist\`：
>
> ```powershell
> Remove-Item -Recurse -Force .\target\release\bundle -ErrorAction SilentlyContinue
> ```
> CI 就是这么做的（`release.yml` 的「构建桌面应用」步有 `rm -rf "target/${{ matrix.target }}/release/bundle"`，注释原文：「cargo 缓存会把上一版的安装包一起带出来，不清理就会混进 Release」）。

### 1.3 cargo 不在默认 PATH 里

症状：`npm run tauri -- build` 报 `failed to run 'cargo metadata' ... program not found`，**而那个报错完全不提 PATH**，极易误判成「Rust 环境坏了」。

`cargo` 由 rustup 装在 `%USERPROFILE%\.cargo\bin\`，把它加进 PATH 即可：

```powershell
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
cargo --version      # 先自证
```

> ⚠ **受限沙箱下 `Test-Path` 对这个目录会返回 `False`（访问被拒）**，据此会误判成"工具链不存在"。判据是**直接执行 `cargo --version`**，不是 `Test-Path`。
>
> ⚠ 历史文档里写的是 rustup 工具链目录（`%USERPROFILE%\.rustup\toolchains\stable-<host>\bin`）。两者都能用；`\.cargo\bin` 是 shim，会跟随 `rustup show` 的默认工具链，**推荐用它**（工具链升级后不用改命令）。

### 1.4 打 tag → CI 自动发布

```powershell
git tag -a v1.0.47 -m "1.0.47: <一句话>"
git push <remote> v1.0.47
```

`.github\workflows\release.yml` 由 **`v*` tag**（或手动 `workflow_dispatch`）触发。结构是**三段**：

| job | 作用 | 关键约束 |
|---|---|---|
| `preflight`（ubuntu-22.04） | **发版前验证门**：核心单测 + 网关单测 + 宿主 WebUI 服务端单测 + 全 workspace 编译校验 | **必须绿**。它一挂，`build` 被 skip、`release` 也不执行 ⇒ 「tag 打上去什么都不会发生」 |
| `build`（4 腿矩阵：windows-x64 / macos-arm64 / macos-x64 / linux-x64） | 各平台构建 + 产物数量断言 | `continue-on-error: true` + `fail-fast: false` ⇒ **单平台失败不阻断其余平台** |
| `release` | 合并 updater 清单 + 建 Release | `if: !cancelled() && needs.preflight.result == 'success' && startsWith(github.ref, 'refs/tags/v')` |

**`release` 的 `if` 三个条件缺一不可**（源码注释写明了为什么）：

- 少了 `!cancelled()`：某平台失败 ⇒ `release` 被 skip ⇒ **一个资产都发不出来**（v0.7.1 就是这样）。
- 少了 `needs.preflight.result == 'success'`：`preflight` 挂了 ⇒ `build` 被 skip ⇒ 而 `!cancelled()` 对「被 skip」是 **true** ⇒ `release` 照常执行却**没有产物**，轻则报错重则发一个空 Release。

**两个必须知道的 CI 行为**：

1. **预发布版本（tag 名含连字符，如 `v1.0.47-beta.1`）会自动去掉 msi**。原因：MSI 的 `ProductVersion` 只接受 `major.minor.build` 三段**数字**，`beta` 这类字母标识**是 MSI 格式自身的限制，改 Tauri 配置绕不过去**。NSIS 用字符串版本号，没这个约束。
2. **预发布必须被标成 prerelease**（workflow 按「版本号里有没有连字符」自动判定）。判据在源码注释里写得很清楚：客户端的 updater 端点是 `releases/latest/download/latest.json`，而 GitHub 的 `releases/latest` **会跳过 prerelease** ⇒ 标对了，正式版用户不会被推到 beta；标错了，**所有正式版用户下次启动就会被更新到 beta**。

**重跑同一个 tag 时必须先删掉已有的 Release**：`softprops/action-gh-release@v2` 对**已存在**的 Release **不覆盖正文**（官方文档原文：「When the release info keys are not explicitly set and there is already an existing release, the release will retain its original info.」）⇒ 「改发布说明 → 重打 tag → 修复说明」这条自救路径会**静默失效**：workflow 全绿、日志无异常，而页面上挂的还是上一次的正文。workflow 里已加了「先 `gh release delete --cleanup-tag=false`」这一步来兜住。

**发布说明**：由 GitHub 按提交列表自动生成。

**发布前必做**：`gh run list` 看一眼 **CI 是不是绿的**。历史上 `ci` 与 `release` 两条工作流**从 1.0.23 起每一次都是红的**（最老的根因可追到 1.0.9），而发版一直手工做 ⇒ 长期没人发现。

### 1.5 版本号 ↔ 安装包覆盖：NSIS 同版本**不覆盖**

`src-tauri/installer.nsi:239` 用 `nsis_tauri_utils::SemverCompare` 比注册表里的 `DisplayVersion`：相等走**维护页**（"已安装/重新安装"），不相等才走升级路径。

⇒ **升版本号是交付的硬前提**。同一版本号反复出包，用户装了不会替换。

**回退（降级）是被允许的**：`allowDowngrades` 未在 `tauri.conf.json` 里显式配置，而 tauri 的默认值是 `true`（`tauri-utils` 的 `NsisConfig::default()`）⇒ NSIS 模板里 `ALLOWDOWNGRADES == "false"` 的分支不生效，降级走正常安装流程。详见 §5。

### 1.6 updater 清单（`latest.json`）

桌面端「检查更新」读的是 `releases/latest/download/latest.json`。它的写法有**三条非显然规则**：

1. **`signature` 字段 = `.sig` 文件的**内容逐字**。`.sig` 本身已经是 base64 文本 ⇒ **原样填入，不要解码、不要二次 base64**。
2. **安装包上传前要改名**成固定名 `ai-gateway-windows-x86_64-setup.exe`。原因：updater 要求 URL 固定，而原始产物名里带版本号（`AI Gateway_<ver>_x64-setup.exe`）。
3. **`platforms` 至少要发两个键**：`windows-x86_64-nsis`（Tauri 2 的 windows target 键）与 `windows-x86_64`（裸键，兼容旧客户端读取逻辑）。两者的 `signature` 与 `url` 相同。

> ★ **改名后不需要重新签名。** minisign 验签只覆盖**文件内容**，签名里 trusted comment 的 `file:` 字段不参与校验 —— 已实测（pynacl 对真实产物验签）：用旧名签出的 `.sig` 能验过改名后的同一文件。
>
> ⚠ 这条与 `scripts/publish-release.sh` **文件头注释**里的说法（"必须对重命名后的文件重新签名，否则客户端校验失败"）**相矛盾**。以 workflow 的实测结论为准，`publish-release.sh` 的注释是**旧口径**。

**生成清单的脚本是 POSIX 的**：`scripts/gen-update-json.sh`（bash）、`scripts/publish-release.sh`（bash + `python3` + `gh`）、`scripts/merge-update-manifests.py`（Python 3）。它们**在 CI 的 ubuntu runner 上跑**；本机是 Windows + 仓库 Shell Policy 强制 PowerShell ⇒ **不要在本机照抄这几个命令**。本机手工发版请用 PowerShell 直接写 JSON。

---

## 2. 签名密钥与常见卡点

### 2.1 密钥在哪：脚本默认探测两处，**本机两处都没有**

`build-signed.ps1` 的 `Resolve-KeyPath`（源码 `:74-94`）按**固定顺序**探测：

| 顺序 | 路径 | 语义 |
|---|---|---|
| 1（首选） | `%USERPROFILE%\.wb-switch\wb-switch-updater.key` / `.password` | 与**现行数据目录**同处（`~/.wb-switch`，见 §3.1） |
| 2（回退） | `%USERPROFILE%\.ai-gateway\ai-gateway-updater.key` / `.password` | 更名那一代（1.0.0~1.0.1）用过的路径 |
| 3（都没有） | 返回**首选路径** | 让后续存在性检查报出面向首选路径的错误 |

走到回退路径时会**打印黄色提示**，但**刻意不自动搬运私钥**。源码注释给的理由值得记住：

> 私钥丢失即无法再为已发布客户端签名新版本，而脚本自动搬运私钥是个不必要的风险面。

**本机实测（2026-10-15）**：`%USERPROFILE%\.wb-switch\wb-switch-updater.key` 与 `%USERPROFILE%\.ai-gateway\ai-gateway-updater.key` **两个都不存在**（`Test-Path` 均为 `False`）⇒ **本机必须显式传 `-KeyFile` / `-PasswordFile`**，否则 `[1/4]` 直接抛「未找到签名私钥」。

密钥实际存放在 `wt-port` **之外**、**仓库根的同级** `keys\` 目录里（`.gitignore` 排除 `*.key` / `*.key.pub`）：

```
<仓库同级>\keys\
  ai-gateway-updater.key         minisign 私钥
  ai-gateway-updater.password    私钥口令
  ai-gateway-updater.key.pub     对应公钥
```

**为什么文档只写"同级"而不写绝对路径**：判据是「别人 clone 到是否成立」。含盘符与用户名的本机路径只对本机成立，写死会让文档在别人机器上是错的 —— 所以命令里用 `Split-Path -Parent $repoRoot` 拼（见 §1.2 的 ②）。**本文档自身也不写这种字面量**（它会命中隐私扫描，也会让「不写本机路径」这条规矩自相矛盾）。

> ⚠ **本仓库是公开仓库，严禁把口令写入任何被 git 跟踪的文件**，也不要把它复制进仓库目录。

> ⚠ `release.yml` 顶部注释里写的 `%USERPROFILE%\.ai-gateway\ai-gateway-updater.key` 是**旧口径**（脚本首选已改回 `.wb-switch`）；`publish-release.sh:45` 的 `${HOME}/.ai-gateway/ai-gateway-updater.key` 同样是旧口径。**以 `build-signed.ps1` 的代码为准。**

### 2.2 坑一：`TAURI_SIGNING_PRIVATE_KEY` 必须是**内容**，不是路径

传路径会报：

```
failed to decode base64 secret key: Invalid symbol 58
```

`58` 是 ASCII 的冒号 —— **路径里的 `:`**（如 `D:\...`）被当成 base64 字符解了。**这个报错完全不提"你传了路径"**，很容易误判成"密钥文件坏了"。

正确读法（脚本内部就是这么做的）：

```powershell
$env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content -LiteralPath $key -Raw).Trim()
```

### 2.3 坑二：缺口令**不报错，而是阻塞等 stdin**

`tauri signer sign` 在**完全缺失**口令时**不抛错**，而是**阻塞等待键盘输入**。表现是「构建卡住 / 构建很慢」——而它其实永远等不到。

**口令错误**才会秒级报错。两者行为相反，这就是为什么「构建卡住」不能靠等：

- 卡住 ⇒ 大概率是**口令没读到**（文件不存在 / 内容为空）；
- 秒失败 ⇒ 大概率是**口令不对**。

`build-signed.ps1` 的 `[2/4]` 步为此专门做了防护：把 stdin 重定向掉（**不给 stdin 机会**）+ **60 秒超时**，确保任何情况下都不会永久挂起。

### 2.4 坑三：加密私钥的 keynum 偏移与公钥**不同**，不能直接比对

- 公钥的 keyid 在解出 blob 的**偏移 `2..10`**；
- **加密私钥**把 keynum 放在 **scrypt 参数之后（偏移 `54..62`）**。

⇒ **拿私钥文件里的 keynum 直接和公钥比，必然不匹配**，据此会误判成「密钥不配对」。

**唯一可靠的判据是实际签一次**：`tauri signer` 用私钥解密后产出的签名，其 keyid **必然**等于对应公钥的 keyid。`-CheckOnly` 做的就是这个（临时目录签一个探针文件 → 解析 `.sig` 的 keyid → 与 `tauri.conf.json` 的 `plugins.updater.pubkey` 比对）。

**本机 keyid（2026-10-15 独立复算）**：

| 对象 | 值 |
|---|---|
| 原始字节序（小端存储） | `7e7ce64b6fc3a36f` |
| minisign 展示形（大端） | `6FA3C36F4BE67C7E` |

两者是**同一把钥匙的两个写法**。复算方式：把 `tauri.conf.json` 的 `plugins.updater.pubkey` base64 解码 → 取第二行再 base64 解码 → 取 `[2..10]` 字节转 hex。

> ⚠ **`217C0E2B4321D841` 是错的**（旧文档里的值）。任何文档里出现它都是错的。
> ⚠ 历史上「私钥与公钥不配对」这个结论也**是错的** —— 误判根源正是上面 §2.4 的偏移问题：把同一把钥匙的两个写法当成了两把。端到端验证（Ed25519 `crypto.verify`）已证明配对正确。

### 2.5 交付前自检：解包比对，**不要安装**

内嵌网关是主程序 exe 内的一个 **gzip 流**。定位 `1F 8B 08` 魔数后解压，应以 `4D 5A`（MZ）开头且 > 1 MB，再与源码编译结果哈希比对：

```powershell
# 与 embedded/gateway.exe.sha256 比对（脚本已把指纹落盘）
$want = (Get-Content .\crates\ai-gateway-core\embedded\gateway.exe.sha256 -Raw).Trim()
$exe  = ".\target\release\ai-gateway.exe"
$bytes = [System.IO.File]::ReadAllBytes($exe)
$off = -1
for ($i = 0; $i -lt $bytes.Length - 3; $i++) {
    if ($bytes[$i] -eq 0x1F -and $bytes[$i+1] -eq 0x8B -and $bytes[$i+2] -eq 0x08) { $off = $i; break }
}
if ($off -lt 0) { throw "未找到内嵌网关（gzip 魔数 1F 8B 08）" }
$ms = New-Object System.IO.MemoryStream
$ms.Write($bytes, $off, $bytes.Length - $off)
$ms.Position = 0
$gz  = New-Object System.IO.Compression.GZipStream($ms, [System.IO.Compression.CompressionMode]::Decompress)
$out = New-Object System.IO.MemoryStream
$gz.CopyTo($out)
$inner = $out.ToArray()
"内嵌网关 $($inner.Length) 字节  头两字节 = 0x{0:X2} 0x{1:X2}" -f $inner[0], $inner[1]
$h = (Get-FileHash -InputStream (New-Object System.IO.MemoryStream(,$inner)) -Algorithm SHA256).Hash
"解出 = $h"
"期望 = $want"
"一致 = " + ($h -eq $want)
```

---

## 3. 在本机跑起来（独立实例验证）

### 3.1 先分清三种"数据目录"

| 名字 | 现行？ | 说明 |
|---|---|---|
| `%USERPROFILE%\.wb-switch\` | ✅ **现行** | `config::store_dir()` 的返回值。**桌面版与 0.8.x 老线共用同一份账号库与网关状态**（有意如此） |
| `%USERPROFILE%\.ai-gateway\` | ❌ **不再写入** | 更名那一代（1.0.0~1.0.1）用过；现在**只作为一次性迁移来源**存在 |
| `$env:AI_GATEWAY_HOME` | ✅ 覆盖开关 | 变量名**刻意不改**（它只有新版本才有，改名只会让既有的隔离脚本失效） |

**为什么目录名回到 `.wb-switch`**（源码 `config.rs:261-275`）：1.0.0 更名时把数据目录换成了 `~/.ai-gateway` 并做了**一次性**复制式迁移，幂等标记 `.migrated-from-wb-switch` 让它**只跑一次** ⇒ 此后 `.ai-gateway` 就成了那一刻的**快照**，用户在任一版本里新增的账号都不会出现在另一个版本里。而两版并行维护、会装在同一台机器上 ⇒ 用户视角这是同一个应用的两代，理当共用同一份账号库。

⇒ **`.ai-gateway` 里如果有数据，会被自动迁回 `.wb-switch`**（`migrate_store.rs`，`LEGACY_DIR_NAME = ".ai-gateway"`）。做隔离验证时**不要**去动这两个真实目录，一律用 `AI_GATEWAY_HOME` 指到别处。

> ⚠⚠ **不设 `AI_GATEWAY_HOME` 就跑，不只是「用了真实数据目录」** —— 它还会**触发网关重启**，也就是会**掐断所有者正在进行的对话**。本仓库有过真实事故记录：所有者的 `:7864` 网关 PID 被从 `24900` 换成了 `42576`。**每次起实例前先确认 `AI_GATEWAY_HOME` 已设且指向独立目录。**

### 3.2 陷阱：`target\release\ai-gateway.exe` 是**共享路径**

`crates/ai-gateway-server`（server 版，约 15 MB）与 `src-tauri`（GUI，约 30 MB）的 `[[bin]] name` **都叫 `ai-gateway`**，输出到同一个 `target\release\ai-gateway.exe`，**互相覆盖**。

后果：直接拿它做接口测试可能拿到 **GUI** —— 而 GUI 第一个注册的插件是单实例保护（互斥量 `com.momo0410.aigateway-sim`，与 `tauri.conf.json` 的 `identifier` 同源），发现所有者的实例在跑就**自己静默退出**（无输出、不监听）⇒ 极易误判成「代码启动即退出」。

**判据是体积**：约 15 MB = server 版（能用）；约 30 MB = GUI（别拿来做接口测试）。

### 3.3 完整命令序列：独立名 + 独立端口 + 独立数据目录

```powershell
# ── 0) 自检：所有者的服务必须在跑（不在就立即停手并报告） ──────────────
foreach ($port in @(7864, 3080)) {
    $c = Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue
    if (-not $c) { Write-Warning "所有者的 :$port 未在运行 —— 立即停止操作并报告" }
}

# ── 1) 用**独立 target 目录**构建 server 版，避免覆盖 GUI 产物 ─────────
$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"
$tdir = "<仓库同级>\test-bin\build-server"
cargo build --release -p ai-gateway-server --target-dir $tdir
# 体积自证：约 15 MB = server；约 30 MB = GUI，别用
(Get-Item "$tdir\release\ai-gateway.exe").Length / 1MB

# ── 2) 改名：确保与所有者的 ai-gateway.exe / gateway-*.exe 都不同名 ─────
$bin = "<仓库同级>\test-bin\wb2api-server.exe"
Copy-Item "$tdir\release\ai-gateway.exe" $bin -Force

# ── 3) 独立数据目录 + 独立端口（避开 7864 / 3080 / 57890，用 5789x） ──
$home1 = "<仓库同级>\test-bin\inst-runbook"
New-Item -ItemType Directory -Force -Path $home1 | Out-Null
$env:AI_GATEWAY_HOME = $home1
$p = Start-Process -FilePath $bin -ArgumentList "serve","--port","57899","--no-open" `
     -PassThru -WindowStyle Hidden `
     -RedirectStandardOutput "$home1\o.log" -RedirectStandardError "$home1\e.log"
"PID=$($p.Id)" | Out-File "$home1\pid.txt" -Encoding ascii

# ── 4) 就绪判据：stdout 出现 webui: 行 ─────────────────────────────────
Start-Sleep -Seconds 3
Get-Content "$home1\o.log"
Get-NetTCPConnection -LocalPort 57899 -State Listen -ErrorAction SilentlyContinue
```

**命令行接口**（`crates/ai-gateway-server/src/main.rs`，与 [SERVER-DEPLOY-1.0.46.md](SERVER-DEPLOY-1.0.46.md) §4 一致）：

| 命令 | 行为 |
|---|---|
| （无参数） | 等同 `serve` |
| `serve` | 可选 `--port <n>`、`--no-open` |
| `version` / `--version` / `-V` | 打印 `ai-gateway <版本>` 后退出 |
| `status` | 打印版本、WorkBuddy 是否运行中、当前账号、账号数后退出 |

> ⚠ **未知参数不会报错**，会被当成 `serve` 处理（`main.rs` 的 `"serve" | _ =>`）。不要靠未知开关表达意图。
> ⚠ **`ai-gateway-server` 的启动横幅版本串可能是旧值**（该 crate 自己的版本常量历史上没跟升）。**唯一可信的版本来源是 `version` 子命令**，不是横幅、更不是 exe 属性页（该产物的 PE 版本资源是空的）。

**发起真实请求**（PowerShell 的引号陷阱必须绕开）：

```powershell
# ❌ 错：PowerShell 用单引号包 JSON 会吃掉内部双引号 ⇒ 请求体变成非法 JSON
# ✅ 对：先把请求体写成 UTF-8 文件，再用 --data-binary "@文件"
$json = '{"model":"<模型名>","messages":[{"role":"user","content":"hi"}],"stream":false}'
[System.IO.File]::WriteAllText("$home1\req.json", $json, (New-Object System.Text.UTF8Encoding($false)))
curl.exe -s -X POST "http://127.0.0.1:57899/v1/chat/completions" `
  -H "Authorization: Bearer sk-123" -H "Content-Type: application/json" `
  --data-binary "@$home1\req.json" --max-time 60 -w "`n[HTTP %{http_code}]"
```

### 3.4 清理：只按 **PID + 路径双条件**

```powershell
$pid1 = (Get-Content "$home1\pid.txt") -replace 'PID=',''
$proc = Get-CimInstance Win32_Process -Filter "ProcessId=$pid1" -ErrorAction SilentlyContinue
if ($proc -and $proc.ExecutablePath -like "*test-bin*") {
    Stop-Process -Id $pid1 -Force
} else {
    Write-Warning "PID $pid1 不是本实例（路径 $($proc.ExecutablePath)）—— 不动它"
}
```

> ⚠ **禁止** `Get-Process -Name "gateway*" | Stop-Process` 之类的按名批量杀。
> ⚠ 网关子进程被宿主纳入 **Job Object**（`KILL_ON_JOB_CLOSE`）⇒ **结束宿主进程，网关会随系统一起退出**，不需要单独去杀它。

### 3.5 要跑 GUI 侧的代码：改 `identifier` 的独立构建

GUI 的单实例锁来自 `identifier`。**不改产品文件**，用一个 override 文件另建一份：

```powershell
# override.json: {"identifier":"com.test.warmup.probe","bundle":{"createUpdaterArtifacts":false}}
npx tauri build --no-bundle --config "<路径>\override.json"
```

独立 `identifier` ⇒ 独立单实例锁 + 独立 WebView2 数据目录 ⇒ 与所有者的实例并存互不干扰。再配 `AI_GATEWAY_HOME` 指向独立目录、端口也换掉。

> ⚠ **GUI 的 stdout/stderr 不进 PowerShell 管道** —— 必须显式重定向才看得到日志：
> ```powershell
> Start-Process -FilePath "<exe>" -PassThru -WindowStyle Hidden `
>   -RedirectStandardOutput "$log\out.log" -RedirectStandardError "$log\err.log"
> ```

### 3.6 前端/UI 验证不需要起后端（⚠ 前提当前不成立）

原设计是用 `uitest/page-harness.cjs` + `uitest/mock-host-api.cjs`（静态服务 + mock 宿主 API + CDP 驱动真实 Chrome）。

**2026-10-15 更正：这两个文件当前不存在。** `git ls-files uitest` → 0 个文件；`git log --all --diff-filter=A -- uitest/page-harness.cjs` → 无任何历史提交；全仓库 find 这两个文件名 → 无匹配。即它们**从未入库**（`uitest/` 是未跟踪的本地目录，现只有 22 个 `.log` 证据日志）。

⇒ **想按这条路做前端验证，需先重建这两个探针**；在它们存在之前，本节步骤**不可执行**。

同批消失的还有 `uitest/README-验证约定.md`（本节与根 `AGENTS.md` 都引用它）。`crates/ai-gateway-server/src/api.rs` 的注释同样引用了 `uitest/mock-host-api.cjs`，亦为悬空。

**为什么它们不在**：`uitest/` 与 `test-bin/` **都不入库**（`.gitignore`），理由是 ① 体积（每个隔离实例都带一份网关副本，12 MB 量级）② **泄漏面**（侦察报告与探针脚本可能夹带真实账号标识）。⇒ 这类「只在本机存在」的验证装置，换台机器就没了，**别把它当成仓库的一部分来依赖**。

---

## 4. 按现象排障

排障前先做两件事：**确认自己在看哪一份产物**、**确认日志在哪**。

| 日志 | 路径 | 说明 |
|---|---|---|
| 网关日志（**排障第一现场**） | `<数据目录>\gateway\gateway_data\gateway.log` | 超过上限轮转为 `gateway.log.1`，**只保留两份** |
| 宿主 stdout/stderr | 前台直接看；后台必须自行重定向 | GUI 的 stdout/stderr **不进 PowerShell 管道** |

### 4.1 「对话输出完就中断」/ 回答被截断

**现象**：内容已经完整输出，界面却报「本轮运行失败」并重试；或者只吐了半句就断。

**根因（两层，分别对应两个版本）**：

- **1.0.34 之前**：上游的收尾顺序是「**先**宣告 `finish_reason`、**后**写终止字节 `[DONE]`」，两者之间那一瞬恰是连接最容易被掐断的位置。旧判据 `if saw_done || !saw_any_frame` **只看 `[DONE]`** ⇒ 「内容已完整、只差 `[DONE]`」落进 else ⇒ **把一份写完的回答判成本轮失败**（网关补一帧 error → 客户端显示「本轮运行失败」）。
- **1.0.43 之前**：上游会在流中间发一帧 `finish_reason` 为**失败值**的负载（HTTP 200 + `choices[0].finish_reason:"error"`，帧里**没有** `error` 字段），或直接发一帧 error 负载 ⇒ 失败帧被**静默当成正常收尾**。

**怎么确认**：读 `gateway.log`，找这两类行：

```
上游流读取失败…saw_done=false
流结束（已收 N 个有效帧 / M 字节，saw_done=…，saw_finish_reason=…）
```

第二类是 1.0.35 才加的（此前流**正常结束**时一行日志都没有，"网关有没有把这条流收干净"根本无法回答）。

**现在的判据**（`upstream/sse.rs` 的 `premature_end()`，六种情形）：

| 情形 | 判定 |
|---|---|
| 见过 `[DONE]` | 正常收尾 |
| 没见过 `[DONE]`，且**一个 data 帧都没读到**（空流） | 正常收尾（客户端得到合法空回合，无损） |
| 没见过 `[DONE]`，但**已经吐过内容** | **失败** |
| 见过**非空** `finish_reason`、只差 `[DONE]` | **正常收尾**（1.0.34 的放宽） |
| 见过**失败语义**的 `finish_reason`（`"error"`） | **失败**（1.0.43 新增；它被排除、**不**点亮 `saw_finish_reason`） |
| `finish_reason` 是**空串** | **不算**宣告完成（空串当结束信号会让客户端一直等） |

**怎么修**：

- 若是**真截断**（半句就断 / 只吐一帧 / 首帧前就断）⇒ **不要放宽判据**。1.0.34 的语义是**单向放宽**，这几类仍会报错并**换账号重试**。这是有意保留的。
- 若是**上游整体空流**（多个不同账号、同一通道都吐不出内容）⇒ 网关自 1.0.40/1.0.41 起报 `upstream_stream_stalled`（**不是** `no_healthy_account`），并且**不再**把空流记进账号熔断（账号无辜）。**客户端应该"稍等片刻再重试"，不要立即并发重试** —— 现场正是 5 路重试 × 全轮转把 13 个号全拖进冷却。
- ⚠ **不要指望网关能修掉链路抖动**。本仓库的结论是：断流本身由**链路/代理抖动**引起（16 次断流集中在请求发出后 32 ms – 6 s、多会话几十至几百毫秒内齐断、与账号/模型/代理节点都无关）。网关只能保证「内容已完整时不误判为失败」。

### 4.2 裸名模型 400 `model_not_in_region` / `model_not_in_upstream`

**现象**：发**不带平台前缀**的模型名（如 `GLM-5.3`、`space-bunny`）报 400，而带前缀（`qoder:Qwen3.8-Flash`）正常。

**这是两个不同的错误码，出路相反** —— 源码注释专门警告过「混用会让用户反复去配置页加一个不存在的模型」：

| 错误码 | 含义 | 出路 |
|---|---|---|
| `model_not_in_region` | 模型存在，但**选中的账号不在能服务它的区域**（上游 11102） | 换模型 **或加区域前缀** |
| `model_not_in_upstream` | **上游清单里根本没有这个模型** | 换模型（白名单放行了也没用） |
| `model_not_in_product` | 用户白名单**没放行**这个「平台 × 模型」 | 去「放行模型」里加 |

**根因（历史上反复出现的一类）**：**「猜测」与「声明」被混在一起参与选号**。

- 网关用来判断"哪个产品提供该模型"的清单有**两份**，语义严格区分：

| 字段 | 来源 | 参与 |
|---|---|---|
| `productModelSet`（上游**真实查询**） | 宿主透传 | 「不排除」**+**「声明」 |
| `productModelFallbackSet`（网关内置**静态表**） | 网关自己的猜测 | **只**参与「不排除」 |

- 一旦"猜测"被当成"声明"，账号多的产品就会**压倒**真正提供该模型的平台 ⇒ 选中一个其实没有该模型的账号 ⇒ 400。

**怎么确认**：

1. 看 `gateway.log` 里的错误码与 message —— 真实原因同时**逐字写在 message 里**，不只看 code。
2. 看 `/v1/models` 与 `/v1/models/regions`：**列表里有 ≠ 抽到的那个号能发**。`/v1/models` 是**并集**（国服独有 / 国际版独有的模型都在里面）。
3. 看是不是"重叠"问题：**多个平台都提供**的模型（`GLM-5.3` / `Auto`）报错，而**单一平台独有**的（`Qwen3.8-Flash`）正常 ⇒ 差别就在"重叠"。

**怎么修**：

- **裸名**路由要靠**上游实测清单**（每次 `GET /v1/models` / `/v1/models/regions` 都刷新；**只放宽"不排除"，永不参与"声明"**）。
- 临时绕过：**用带平台前缀的模型名**（`qoder:` / `zcode:` / `国际版:`），它是用户的**显式指令**，候选集锁死该产品。
- ⚠ **加任何"补全清单"的逻辑前先问一句：它会被当成声明吗？** 若会，就必须走 fallback 那条路 —— **猜测永远不该赢过事实**。

### 4.3 503 `no_healthy_account`

**这个码的语义是「账号池暂时不可用，稍后重试」**。它被**刻意**用在"重试有意义"的场合；凡是"重试无用"的原因都分出了专属码。所以看到 503 `no_healthy_account` 时，**先确认它不是别的码被误显示**。

**按可能性排序的原因**：

| # | 原因 | 怎么确认 | 怎么修 |
|---|---|---|---|
| 1 | **客户端并发重试**把单账号产品打满 | `curl` 单发**每次成功**，客户端**每次失败** | 见下方"单账号产品"段 |
| 2 | **账号池空**（熔断/禁用/冷却） | `GET /status` 看 `in_flight`、账号档位 | 等冷却，或换号 |
| 3 | **上游整体故障**（空流风暴） | `gateway.log` 有 `upstream_stream_stalled` | 稍等片刻再试（**不要**立即并发重试） |
| 4 | **平台总开关关掉了** | 该报 `platform_not_in_routing` | 回账号页打开平台开关 |
| 5 | 裸名模型被"猜测清单"排除 | 见 §4.2 | 用平台前缀绕过 |

**单账号产品（qoder / zcode 各只有 1 个账号）是这类问题的高发区**：

- `max_in_flight` 是**每账号**上限，在单账号产品上就变成了**每产品**上限；
- DSH 用的 `@earendil-works/pi-ai` 带 `retryProviderRequest`，默认**重试 5 次**，而**每个重试是独立的并发请求**；
- **当前值 `max_in_flight = 3`**（三个产品统一，`config.rs` 默认值，有测试钉住）。

> ⚠ **2026-10-15 更正**：本节此前（以及若干注释里）称"名额满时**排队**而非失败（`server.Config.InFlightWait` 默认 3000 ms）"—— **在当前 Rust 实现里不成立**。逐条核实：`Pool::acquire` 满额时**直接返回 `false`**；`forward.rs` 是唯一调用点，返回 false 时**换号重试**；全仓库 `InFlightWait` / `in_flight_wait` 只命中 2 处、**都在注释里、没有实现**。`server.Config.InFlightWait` / `pool.AcquireWait` 是 **Go 版**（`go-gateway/`，已不参与构建）的标识符。
>
> ⇒ 现行语义：**每账号最多 3 个在途；满额时该账号被跳过、请求换号重试**，不存在等待/排队。对**单账号产品**而言换号无处可换 —— 这正是"并发重试成片 503"的成因形状。**保留换号还是补回排队，是需要重新评估的决策**（见 `.agents/notes/implemented/architecture/2026-09-22-max-in-flight-and-account-switch-retry.md`）。

**排查这类问题的方法**（值得记住）：

1. **先怀疑"请求形态不同"** —— 读客户端源码看它到底发什么；
2. **再怀疑"并发"** —— 客户端带重试，而重试往往是并发的；
3. **用并发梯度定位边界** —— 历史上实测出「并发 3 全通、并发 4 起失败」，直接把 `max_in_flight=3` 钉死为根因。

### 4.4 网关没跟着自启

**现象**：重启电脑后 App 起来了，**网关没有**；而日志里**没有任何停止记录**（无停机日志、无 panic、无 Application Error、无 WER）。

**根因（1.0.44 修）**：不是"它怎么死的"，而是"**死了没人管**"。开机后 App 由 `HKCU\...\Run` 拉起，网关起来后约 20 秒被**外力抹掉**（宿主 Job Object + `KILL_ON_JOB_CLOSE` 的内核级终止，那一环**至今未归因**），而 `maybe_autostart` 只在 setup 里跑**一次** ⇒ 之后再没有任何人看过它一眼。

**怎么确认**：

```powershell
# ① 配置里勾了"随 App 启动"吗（界面上的开关 = auto_start）
$cfg = "$env:USERPROFILE\.wb-switch\gateway\gateway_config.json"
(Get-Content $cfg -Raw | ConvertFrom-Json).auto_start

# ② 网关端口上有没有人（默认 7863）
Get-NetTCPConnection -LocalPort 7863 -State Listen -ErrorAction SilentlyContinue

# ③ 探活
curl.exe -s http://127.0.0.1:7863/healthz -w "`n[HTTP %{http_code}]"

# ④ 网关日志的最后一屏
Get-Content "$env:USERPROFILE\.wb-switch\gateway\gateway_data\gateway.log" -Tail 40
```

**现在的机制（三态意图阀）**：

| 状态 | 何时进入 | 行为 |
|---|---|---|
| `UNKNOWN` | 宿主刚起来、还没采样配置 | **只在 `UNKNOWN` 时**采样：`auto_start` 为真 ⇒ 置 `WANTED` |
| `WANTED` | 用户点「启动」/「重启」，或开机自启成功 | 被外力杀掉就**补拉** |
| `STOPPED` | 用户点「停止」 | **绝不**自动补拉（否则用户永远关不掉它） |

补拉由 `run_auto_sync_loop` **每 30 秒一轮**执行。失败有**退避**：同一个失败原因每 `GATEWAY_RETRY_EVERY_ROUNDS = 10` 轮（≈5 分钟）才重试一次，**换了错法立刻重试**。

**怎么修 / 怎么判**：

- **勾了 `auto_start` 但网关没起来** ⇒ 看 `gateway.log` 里 `[gateway] 随 App 启动失败: <原因>`；`gateway_config.json` 的 `last_error` 里也会写。
- **`last_error` 反复是「网关正在启动中，请稍候」** ⇒ 那是**互斥**，不是失败。**注意**：监督函数**不能**先抢启动闸门再调 `start_gateway`（那道闸门不可重入），否则会**永远**补拉不起来 —— 源码里记了 2026-10-13 的实测现场。
- **`last_error` 反复是「端口已被占用」** ⇒ 端口上有**别人的**服务；或已有本程序的网关但宿主没认领。判据与状态页**完全一致**（探活 + `X-Service` 身份校验）。
- **`running` 与 `enabled` 是两个不同字段**：网关正在跑时 `running=true`，而 `enabled` 只是开关状态，实测自动拉起后它**仍可能是 `false`**。**判断存活看 `running` / 端口 / `/healthz`。**

### 4.5 旧网关不自动换新

**现象**：升级了 App，但实际处理请求的仍是**旧网关二进制** —— 新版里的修复（如"上游流中断归因"）永远不生效，用户看到的还是旧文案。

**根因**：端口上已有**上一轮释放的**网关在跑时，`maybe_autostart` 走 `adopt_running_gateway()` **认领**它（这是对的，它解决了"明明在跑却说启动失败"），但认领之后**就不会再释放新版本内嵌的网关**了。

**关键事实：网关进程名里带内容指纹**：

```
gateway-<长度>-<FNV1a>.exe
```

指纹取自**内嵌数据的字节内容** ⇒ **名字相同就意味着内容逐字节相同**（此时重启没有收益，只会白断一次正在服务的连接）；**名字不同 ⇒ 跑的是别的版本 ⇒ 需要换新**。

**怎么确认**：

```powershell
# ① 端口占用者的进程名（形如 gateway-4e52fa-ecd7924c953073e8.exe）
Get-NetTCPConnection -LocalPort 7864 -State Listen -ErrorAction SilentlyContinue |
  Select-Object OwningProcess
Get-CimInstance Win32_Process -Filter "ProcessId=<上面的 PID>" |
  Select-Object ProcessId, Name, ExecutablePath

# ② 数据目录里释放过哪些副本（按内容指纹命名）
Get-ChildItem "$env:USERPROFILE\.wb-switch\gateway\bin\"

# ③ 日志里换新那一行
Select-String -Path "$env:USERPROFILE\.wb-switch\gateway\gateway_data\gateway.log" `
  -Pattern "不是本版本释放的" -Context 0,2
```

**怎么修**：这是**自动**的 —— 确认旧版本后，`maybe_autostart` 会走**串行化重启入口**把它换成新版本。日志会打印：

```
[gateway] 端口 <port> 上的网关不是本版本释放的，正在换成本版本内嵌的网关
```

> ⚠ **只在「网关应当来自内嵌数据」时才做版本比对**。若设了 `AI_GATEWAY_ROUTER_BIN`（手工指定），或用了"同目录自备 `gateway.exe`"，文件名本来就不带指纹，**硬比会把每一次启动都判成"需要换新"并反复重启** —— 那是比版本落后更糟的故障。
> ⚠ **换新会短暂中断正在服务的请求**，所以只在这一种情况下做。
> ⚠ 换新**失败**（旧进程杀不掉等）时**不会**把状态写成 `failed` —— 端口上确实有一个健康的网关在服务，用户看到的应当是「运行中」，只是版本旧一点。失败原因只留在日志里，不制造"明明能用却报失败"的第二副面孔。
> ⚠ **重启必须走 `restart_gateway_serialized()`**，不要自己写 `stop` + `start`（并发交错时后到的那个必然撞上"网关已在运行"检查）。有测试钉住"不得新增绕过锁的裸 `stop_gateway()`"。

### 4.6 内嵌网关不是当次构建的那份

**现象**：**新功能在开发机上好好的，装完却没有**；而构建日志里**没有任何异常**。

**根因**：`crates/ai-gateway-core/build.rs` 会**无条件内嵌** `embedded/gateway.exe` 里当时躺着的那一份。于是"改了网关源码、但没重建 embedded" ⇒ 包内嵌**旧网关**。

**build.rs 的查找顺序（三个候选，取第一个 > 1 MB 的）**：

1. 环境变量 `AI_GATEWAY_ROUTER_BIN`
2. `crates/ai-gateway-core/embedded/gateway[.exe]`
3. 仓库根 `dist/gateway[.exe]`

⚠ 构建日志**只有一行** `cargo:warning=已内嵌网关: <路径>`，**极易忽略**。

**怎么确认（三道判据）**：

```powershell
cd <仓库>\wt-port

# ① 网关产物新鲜度（不构建）
& .\scripts\build-gateway.ps1 -CheckOnly

# ② 单端指纹：构建前后 embedded/gateway.exe 的 SHA256 必须一致（防并发构建）
Get-FileHash .\crates\ai-gateway-core\embedded\gateway.exe -Algorithm SHA256

# ③ 有没有"更高优先级"的候选在截胡
"AI_GATEWAY_ROUTER_BIN = " + $env:AI_GATEWAY_ROUTER_BIN
Test-Path .\dist\gateway.exe
```

**怎么修**：

- **常规**：用 `build-signed.ps1`（第 `[3/4]` 步**强制重建**网关 + 构建后**双端指纹比对**）。**别跳过这一步。**
- **`AI_GATEWAY_ROUTER_BIN` 指向另一份网关** ⇒ 脚本会**直接失败**（因为环境变量**优先于** embedded/，那会让内嵌的不是你刚构建的那份）。**unset 它**，或让它指向同一份产物。
- **`dist/gateway.exe` 与本次内嵌的不同** ⇒ 脚本只**提示**不失败（embedded/ 在查找顺序里**先于** dist/，本次内嵌的仍是 embedded 那份）。但要知道它是个**容易被误认为"生效了"的陷阱**。
- **产物小于 1 MB** ⇒ `build.rs` 会判为「不是有效网关」而**静默跳过内嵌**，产出一个**不内嵌网关的包**。`build-gateway.ps1` 会在这一步直接失败（阈值与 build.rs 一致）。

**单独重建网关（不打包）**：

```powershell
& .\scripts\build-gateway.ps1              # 增量：源码没变则跳过
& .\scripts\build-gateway.ps1 -Force       # 强制重建
& .\scripts\build-gateway.ps1 -CheckOnly   # 只校验新鲜度，不构建
```

> ⚠ **`embedded/` 是构建产物，不入库**（`.gitignore` 排除 `crates/ai-gateway-core/embedded/`）⇒ **clone 后该目录不存在**，必须先跑一次 `build-gateway.ps1`，否则 `build.rs` 只打一行 warning 就继续。
> ⚠ `go-gateway/` 仍留在仓库里作为**对照实现与回归依据**，但**构建链路上已完全不依赖 Go 工具链**。

### 4.7 内嵌前端不是当次构建的那份（**同型缺陷**）

`tauri-build` 的 build script 里 `rerun-if-changed` **不包含** `../dist` ⇒ **前端重建不会触发重新嵌入**，cargo 可能沿用上一次编进二进制的旧前端。

**判据只能是资源文件名**：

```powershell
# dist/index.html 写死了它引用哪个 assets/index-XXXX.js
$want = [regex]::Match((Get-Content .\dist\index.html -Raw), 'assets/index-[A-Za-z0-9_\-]+\.js').Value
# 那个名字是 Vite 按**内容哈希**生成的，且文件名在 exe 里是**明文**（Tauri 只压 JS 内容，不压文件名）
$got = [regex]::Matches([System.Text.Encoding]::ASCII.GetString(
    [System.IO.File]::ReadAllBytes('.\target\release\ai-gateway.exe')), 'assets/index-[A-Za-z0-9_\-]+\.js') |
    ForEach-Object { $_.Value } | Sort-Object -Unique
"dist 引用 = $want"
"exe 内嵌  = $($got -join ', ')"
"一致 = " + ($got -contains $want)
```

> ⚠⚠ **在 exe 里搜业务字符串是错的判据。** 事故现场：所有者报「zcode 这里还是老卡片啊 你是不是没打包？」—— 查证后代码与安装包**都是对的**，只是他装的是更早那一版；而当时用的判据是"在 exe 里搜前端代码里的字符串"，得出**错误结论**，白绕了一大圈。原因：Tauri 对 JS **内容**做了压缩/编码，连**早就存在**的老标识也搜不到。**搜不到 ≠ 没打包。**

**脚本报「内嵌前端不是当次构建的那份」时的修法（按顺序试）**：

1. `cargo clean -p ai-gateway`，然后重跑脚本；
2. 仍不行：删 `target\release\ai-gateway.exe` 再构建；
3. 再不行：`cargo clean` 全量重建（慢，但一定干净）。

### 4.8 构建"卡住"不动

见 §2.3：**缺口令时 `tauri signer sign` 不报错，而是阻塞等 stdin**。

**先跑 `-CheckOnly`**（秒级）就能区分是"密钥/口令问题"还是"真的在编译"。`build-signed.ps1` 的 `[2/4]` 步已用「重定向 stdin + 60 秒超时」把它变成**明确失败**，不再永久挂起。

### 4.9 「检查更新」必然失败（**已知状态，不是缺陷**）

`src-tauri/tauri.conf.json` 的 `plugins.updater.endpoints` 指向：

```
https://github.com/iuuuuuuuu/ai-gateway/releases/latest/download/latest.json
```

而该仓库 **2026-10-06 已按 Owner 指令转为 private** ⇒ **匿名访问 404**。

**关键点**：`tauri-plugin-updater` 是**匿名 HTTP 拉取、不带任何凭据**，所以转私有后**桌面端自动更新彻底失效** —— 包括本机桌面那版。这**不是配置问题，加 `Authorization` 头也救不了**。

**怎么确认**（A/B 实测法，本机同一时刻）：

```powershell
# 匿名拉取：期望 404（private 仓库）
curl.exe -sS -o NUL -w "HTTP %{http_code}`n" `
  "https://github.com/iuuuuuuuu/ai-gateway/releases/latest/download/latest.json"
```

**要恢复只有两条路**：

1. **改回 public**；
2. **把 `endpoints` 换成别的托管点**（自建静态站 / 对象存储 / 另一公开仓库）。

> ⚠ **别把这条当 bug 去查。** 它是**已知状态**：排障时看到"检查更新"失败 + 端点 404，第一反应应当是"仓库是 private"，而不是去翻 updater 配置或签名。

### 4.10 端口被占用

```powershell
# 谁占着
Get-NetTCPConnection -LocalPort 7863 -State Listen | Select-Object OwningProcess
Get-CimInstance Win32_Process -Filter "ProcessId=<PID>" | Select-Object ProcessId, Name, ExecutablePath

# 服务端形态还有 HTTP 接口
curl.exe -s -X POST http://127.0.0.1:57890/api/gateway/port-holder `
  -H "Content-Type: application/json" -d '{\"port\":7863}'
```

**换端口后必须重启网关**：端口在网关**启动时**绑定，改完只有重启才生效。

> ⚠ 服务端形态的启动失败判据：stdout 打印 `启动失败: 端口 <n> 被占用或不可用（...）。可用 --port 指定其他端口。` 并**退出码 1** —— 脚本里应以此判定失败。

### 4.11 客户端 401 / 429

| 现象 | 原因 | 处理 |
|---|---|---|
| 401 `invalid_api_key` | 网关设了 `api_key`，客户端没带或带错 | 鉴权优先读 `Authorization: Bearer <key>`，其次 `x-api-key`；**`api_key` 为空时完全不校验** |
| 429 `rate_limit_exceeded` | 命中**账号级每日用量护栏** | 见下 |

**429 的两个门槛**（都在 `gateway_config.json`，默认 **0/0 = 关闭**）：

| 栏位 | 语义要点 |
|---|---|
| `daily_token_cap` | 计数 = `input + output + cacheWrite`（**不含缓存读取**）；「今日」按**本机本地日期**，**零点自动恢复** |
| `credit_floor` | 积分由巡检刷新；**与日期无关，不会零点恢复**（巡检回充或调低下限才会解除） |

**行为特征**：命中后该账号返回 429、**仍在列表中、不会被禁用**，只是不参与选号；是**选号闸门**不是限流器（最多约 5 秒滞后，**不会中断在途请求**）；改配置后**最长 5 秒热加载生效，不需要重启网关**；同时命中两个栏位时**额度原因优先**。

> ⚠ **`daily_token_cap` / `credit_floor` 必须写在原生配置的顶层。** 写进 `pool` 里**不会报错，但会被静默忽略**（等于没配）。

**自查是否被护栏挡住**：`GET /api/gateway/status` 里对应账号的 `daily_guard` 为 `true`；`gateway\gateway_data\usage.json` 可看到当日计数。

### 4.12 CI 全红而本地全绿

**根因形状**：`test-bin/`、`dist/` 这类**被 `.gitignore` 排除、但测试或构建依赖**的目录 —— 在开发机上永远存在，在 CI 上**根本不存在**。

**判据**：凡是被 `.gitignore` 排除、但**测试或构建依赖**的路径，都必须先问一句「**CI 上它从哪来**」。

**怎么本地复现 CI 的真实条件**（用一个没有 `dist/`、没有 `test-bin/` 的干净 checkout）：

```powershell
git worktree add --detach <临时目录> HEAD
# 在那个目录里跑 cargo test / cargo check
```

> ⚠ 历史上有两条 workflow **从 1.0.23 起每一次都是红的**（最老根因可追到 1.0.9），而发版一直手工做 ⇒ 长期没人发现。**发版前先看 CI 是否绿。**

---

## 5. 回滚

### 5.1 版本回退：装旧版安装包即可（降级是允许的）

`allowDowngrades` 未在 `tauri.conf.json` 显式配置，tauri 的默认值是 `true` ⇒ NSIS 的降级拦截分支**不生效**，装旧版走正常安装流程。

**步骤**：

1. **结束宿主进程**（网关在同一个 Job Object 里会一起退出，不需要单独杀它）；
2. 装目标版本的 `AI Gateway_<ver>_x64-setup.exe`；
3. 重启。

> ⚠ **绝不要用安装包做验证**（见 §0 红线 1）。回滚是**用户操作**，不是开发者验证手段；开发者验证请用 §2.5 的**解包比对**。

### 5.2 回滚**数据目录**：不需要动

- 数据目录 `%USERPROFILE%\.wb-switch\` **不随版本走**，升级/降级都**不动它**。
- 内嵌网关副本在 `gateway\bin\gateway-<内容指纹>.exe`，**按内容指纹命名** ⇒ 换版本会**自动释放新的**，旧副本可留可删。
- ⚠ **但"回滚到旧版本"会把旧版本的网关一起带回来** —— §4.5 的换新机制是**双向**的：装旧版后，端口上那个新网关会被判成"不是本版本释放的"并换成旧的。

### 5.3 回滚版本号 ≠ 回滚功能：**必须知道代价**

**实例（2026-10-07，Owner 实机）**：Owner 回退到 **1.0.6**（他自选的安装目录下的 `ai-gateway.exe`，FileVersion 1.0.6，`:7864` 跑的是 Go 网关）。后果：

- 当次 **1.0.22 – 1.0.34 均未安装** ⇒ 那一批修复全部丢失；
- **回退到 Go 版会丢掉 1.0.21 – 1.0.33 的全部修复**；
- **1.0.34 修的「输出完就中断」在 Go 版里同样存在** —— Go 只在「0 有效帧」时补 error 帧，**吐过内容却没见 `[DONE]` 时静默当成功**，与本仓库 1.0.33 及以前的行为**不同**。

⇒ **回滚前先确认目标版本里有没有你要的那个修复。** 回滚是"退到某个确定的状态"，不是"撤销最后一次改动"。

### 5.4 回滚一次有问题的**配置**

```powershell
# 管理侧配置（界面「网关」页改的就是这份）
$cfg = "$env:USERPROFILE\.wb-switch\gateway\gateway_config.json"

# 备份
Copy-Item $cfg "$cfg.bak-$(Get-Date -Format yyyyMMdd-HHmmss)" -Force

# 改完的生效时机（★ 两种写法不同）
#   · 只改栏位（界面保存 / POST /api/gateway/config） ⇒ 最长 5 秒热加载，不重启网关
#   · 点「启动网关」/ /api/gateway/start、或 /mode /restart ⇒ 立即按新配置重起网关进程
```

> ⚠ `gateway_native_config.json` 是**派生文件**：管理侧一保存/重启就会被**重写**，手改会丢。要持久化请改 `gateway_config.json` 或走界面。

---

## 6. 卸载与清理

### 6.1 ⚠⚠ 红线：**绝不运行卸载器**

包括 `uninstall.exe` 与 `AI-Gateway_*_setup.exe`，**加了 `/S` 更危险**。

**机制**（`src-tauri/installer.nsi` 与 Tauri 的 `utils.nsh` 模板）：安装器/卸载器在写文件前调 `CheckIfAppIsRunning "${MAINBINARYNAME}.exe"`，而它是按**可执行文件名**匹配并结束进程，**不比对路径**：

```nsis
nsis_tauri_utils::FindProcessCurrentUser "${executableName}"
nsis_tauri_utils::KillProcessCurrentUser "${executableName}"
IfSilent kill_${UniqueID} 0        ; 静默模式下不询问，直接杀
```

主程序名 `ai-gateway.exe` 与所有者正在运行的实例**同名** ⇒ 「装一次 / 卸一次」就会杀掉他的实例；而它是 Job Object（`KILL_ON_JOB_CLOSE`）的父进程，**父进程一死，网关被系统连带回收** —— 一次误操作打掉两个服务。

**要验证安装包内容，用解包（§2.5），不要安装。**

### 6.2 卸载器会删什么、**不删**什么

按本仓库的 `installer.nsi` 模板，`Section Uninstall` 在**勾选了"删除应用数据"**且非更新模式下，只额外删除：

```nsis
RmDir /r "$APPDATA\${BUNDLEID}"        ; BUNDLEID = identifier = com.momo0410.aigateway
RmDir /r "$LOCALAPPDATA\${BUNDLEID}"   ; WebView2 用户数据目录
```

⇒ **卸载器不删 `%USERPROFILE%\.wb-switch\`。** 账号库、网关配置、凭证、日志、用量都会**留在原地**。要彻底清干净必须**手工删**：

```powershell
# ⚠ 删之前先确认路径，并把要保留的账号库单独备份
$store = "$env:USERPROFILE\.wb-switch"
Test-Path $store
# 备份（账号库是唯一真源，含可直接登录的凭证）
Copy-Item "$store\accounts.json" "<备份目录>\accounts.json.bak" -Force -ErrorAction SilentlyContinue

# 确认无误后再删（破坏性）
# Remove-Item -Recurse -Force $store
```

> ⚠ **`.ai-gateway` 旧目录**：如果老机器上还有它，里面可能存着更名那一代的数据。现在它**只作为一次性迁移来源**（迁到 `.wb-switch`）。**删之前先确认迁移已经跑过**（`.wb-switch` 里有你的账号），否则会丢数据。
>
> ⚠ **备份/迁移 = 打包整个数据根目录。** 换机器只要账号库跟着走，网关配置与凭证都会**自动重建**。

### 6.3 清理开发期的独立实例

```powershell
# 只按 PID + 路径双条件（见 §3.4）
$proc = Get-CimInstance Win32_Process -Filter "ProcessId=<PID>" -ErrorAction SilentlyContinue
if ($proc -and $proc.ExecutablePath -like "*test-bin*") { Stop-Process -Id <PID> -Force }

# 独立数据目录直接删（它在 test-bin/ 下，不在 ~/.wb-switch）
Remove-Item -Recurse -Force "<仓库同级>\test-bin\inst-runbook"
```

> ⚠ **禁止按进程名批量杀**（`Get-Process -Name "gateway*" | Stop-Process`）。判据永远是 **PID + `ExecutablePath`**。

### 6.4 清理构建产物

```powershell
cd <仓库>\wt-port

# bundle 目录会累积历史版本（构建前就该清，见 §1.2）
Remove-Item -Recurse -Force .\target\release\bundle -ErrorAction SilentlyContinue

# ⚠ 交付目录 dist/ 里还有历史草稿与 md —— 脚本对它"只复制不清理"，别整目录删
Get-ChildItem "<仓库同级>\dist\" | Select-Object Name, Length, LastWriteTime
```

> ⚠ `embedded/gateway.exe` **是构建产物、不入库** ⇒ 删掉它不会"清理出问题"，但**下次构建必须重新跑 `build-gateway.ps1`**，否则 `build.rs` 只会打一行 warning 然后产出一个**不内嵌网关的包**（见 §4.6）。

---

## 附：本文档里"我无法核实而留白"的点

写文档时按「**每条结论都要能在源码/脚本/日志里核对**」执行，以下几条**没有写成结论**，如实留白：

| 项 | 状态 |
|---|---|
| 密钥的**具体绝对路径** | 有意不写。它只对本机成立（判据：别人 clone 到是否成立），且属于凭据位置。文档只写"在 `wt-port` 之外、`WorkbuddySwitchAPi` 之内"与脚本的默认探测规则 |
| 所有者服务端口的**权威清单** | 未写成固定清单。2026-10-15 实测 `:7864`（网关）与 `:3080`（DSH GUI）在听，而 `:43120` **不在听**（`../AGENTS.md` 里写的是 `:43120`）。端口会变，**判据是现场 `Get-NetTCPConnection`**，不是文档里的数字 |
| 本地手工发 Release 的**完整可照抄命令** | 未给。`publish-release.sh` / `gen-update-json.sh` 是 bash + `python3`，与本仓库 Shell Policy 冲突；且依赖 `gh` 的登录态，未登录时不可用 |
| 网关"起来后又悄悄没了"的**根因** | 未写成结论。1.0.44 修的是"死了没人管"，**"它怎么死的"至今未归因**（现场无停机日志、无 panic、无 Application Error、无 WER） |
| `max_in_flight` 该"换号"还是"排队" | **未决**。见 §4.3 的更正框 —— 这是需要 Owner 定夺的决策，文档只陈述现状与影响，不给结论 |
| 前端/UI 验证路径 | **当前不可执行**。`uitest/page-harness.cjs` + `uitest/mock-host-api.cjs`（以及 `uitest/README-验证约定.md`）从未入库，需先重建（§3.6） |
| `scripts/*.ps1` 里 **`-DebugBuild` 参数**的可用性 | 未实测。`build-gateway.ps1` 有它（debug 产物约 22 MB、会显著撑大安装包），但**参数名刻意不叫 `-Debug`** —— `[CmdletBinding()]` 会自动注入 `Debug` 通用参数，重名会让**整个脚本在任何调用下都报** `A parameter with the name 'Debug' was defined multiple times`。这条是**读源码 + 注释**得出的，未实跑验证 |
