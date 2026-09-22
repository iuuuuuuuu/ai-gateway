<#
.SYNOPSIS
    构建带 updater 签名的桌面安装包（exe/msi），并把签名密钥的定位与校验固化下来。

.DESCRIPTION
    为什么需要这个脚本：
      tauri.conf.json 里 createUpdaterArtifacts=true，因此 `tauri build` **必须**拿到
      minisign 私钥，否则构建在最后一步失败。而私钥按设计不入库（.gitignore 排除
      *.key），AI 与新同事每次都要全盘搜索，且下面两个坑会把它拖成"大量时间"：

        1) 口令缺失时 `tauri signer sign` **不报错，而是阻塞等待 stdin 输入**，
           看起来像"构建很慢"。本脚本改为构建前先校验口令，缺失立即退出。
        2) TAURI_SIGNING_PRIVATE_KEY 必须是密钥**内容**，传文件路径会报
           `Invalid symbol 58`（路径里的冒号）。本脚本统一用 -Raw 读内容。

    因此本脚本把"找密钥"变成"不用找"：密钥路径固定，构建前先做 keyid 预检，
    不匹配就秒级失败，绝不浪费一次完整构建。

    私钥与口令的固定位置（均在仓库外，公开仓库零提交风险）：
      %USERPROFILE%\.wb-switch\wb-switch-updater.key       minisign 私钥
      %USERPROFILE%\.wb-switch\wb-switch-updater.password  私钥口令

.PARAMETER Bundles
    要构建的 bundle 类型，默认 "nsis"（Windows 安装包）。多平台用逗号分隔，如 "nsis,msi"。

.PARAMETER KeyFile
    私钥文件路径，默认 %USERPROFILE%\.wb-switch\wb-switch-updater.key。

.PARAMETER PasswordFile
    口令文件路径，默认 %USERPROFILE%\.wb-switch\wb-switch-updater.password。

.PARAMETER CheckOnly
    只做密钥预检（路径、口令、keyid 与 tauri.conf.json 公钥是否配对），不执行构建。
    适合在动辄十几分钟的构建前先确认密钥可用。

.EXAMPLE
    pwsh scripts/build-signed.ps1
    构建 Windows nsis 安装包并签名。

.EXAMPLE
    pwsh scripts/build-signed.ps1 -CheckOnly
    仅校验私钥与口令是否可用、是否与配置公钥配对。

.EXAMPLE
    pwsh scripts/build-signed.ps1 -Bundles "nsis,msi"
    同时构建 NSIS 与 MSI。
#>
[CmdletBinding()]
param(
    [string]$Bundles = "nsis",
    [string]$KeyFile = "",
    [string]$PasswordFile = "",
    [switch]$CheckOnly,
    # 跳过 Go 网关重建，沿用 embedded/ 下现有的产物。
    #
    # 只建议在"确定网关没动、只想快速重打前端/UI"时使用 —— 它会**放弃**
    # 本次构建对"内嵌网关新鲜度"的保证（校验仍会跑，但基准是旧产物）。
    [switch]$SkipGateway
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot   # 仓库根

# ── 密钥路径解析（含旧路径回退）─────────────────────────────────────────
#
# 2026-09-16 拆分后，数据目录统一回 ~/.wb-switch（见 config::store_dir 的注释：
# 两版并行维护、共用同一份账号库），因此密钥的**首选路径也回到 ~/.wb-switch**。
# ~/.ai-gateway 是更名那一代（1.0.0~1.0.1）用过的路径，作为回退保留 ——
# 只在其中一处放过密钥的机器仍能签出包。
#
# 刻意不自动移动密钥文件：私钥丢失即无法再为已发布客户端签名新版本，
# 而脚本自动搬运私钥是个不必要的风险面。改为「首选路径优先、另一路径回退」，
# 并在用到非首选路径时明确提示用户，由用户自行决定是否迁移。
function Resolve-KeyPath {
    param(
        [string]$Explicit,
        [string]$NewName,
        [string]$LegacyName
    )
    if ($Explicit) { return $Explicit }
    $new = Join-Path $env:USERPROFILE ".wb-switch\$LegacyName"
    if (Test-Path -LiteralPath $new) { return $new }
    $legacy = Join-Path $env:USERPROFILE ".ai-gateway\$NewName"
    if (Test-Path -LiteralPath $legacy) { return $legacy }
    # 两者都不存在：返回首选路径，让后续的存在性检查给出面向首选路径的报错
    return $new
}

if (-not $KeyFile) {
    $KeyFile = Resolve-KeyPath -Explicit "" -NewName "ai-gateway-updater.key" -LegacyName "wb-switch-updater.key"
}
if (-not $PasswordFile) {
    $PasswordFile = Resolve-KeyPath -Explicit "" -NewName "ai-gateway-updater.password" -LegacyName "wb-switch-updater.password"
}

if ($KeyFile -match '\.ai-gateway\\') {
    Write-Host "提示：正在使用更名那一代的密钥路径 $KeyFile" -ForegroundColor Yellow
    Write-Host "      如需迁移到当前路径，请手动把 .key 与 .password 复制到 %USERPROFILE%\.wb-switch\ 并改名为 wb-switch-updater.*" -ForegroundColor Yellow
    Write-Host "      （脚本刻意不自动搬运私钥：私钥丢失即无法再为已发布客户端签名）" -ForegroundColor Yellow
}

# ── 工具函数 ────────────────────────────────────────────────────────────

# 从 tauri/minisign 的 base64 载荷中取出 keyid（第 2 行 base64 的第 2..10 字节，小写 hex）。
# 私钥、公钥、签名三者格式一致，因此可用同一个函数比对，这是判断"这把私钥能不能
# 给这个客户端签名"的唯一可靠依据。
function Get-MinisignKeyId {
    param([Parameter(Mandatory)][string]$Base64Payload)

    $outer = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Base64Payload.Trim()))
    $lines = $outer.Trim() -split "`n"
    if ($lines.Count -lt 2) { throw "载荷格式异常：解出后不足两行" }
    $inner = [Convert]::FromBase64String($lines[1].Trim())
    if ($inner.Length -lt 10) { throw "载荷格式异常：内层不足 10 字节" }
    return (($inner[2..9] | ForEach-Object { $_.ToString('x2') }) -join '')
}

function Get-KeyKind {
    param([Parameter(Mandatory)][string]$Base64Payload)
    $outer = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Base64Payload.Trim()))
    return ($outer.Trim() -split "`n")[0].Trim()
}

# ── 1) 定位私钥 ────────────────────────────────────────────────────────
Write-Host "==> [1/4] 检查签名私钥" -ForegroundColor Cyan

if (-not (Test-Path -LiteralPath $KeyFile)) {
    throw @"
未找到签名私钥：$KeyFile

私钥不入库（.gitignore 排除 *.key），需要放在上述固定路径。
可用 -KeyFile 指定其他位置；若私钥已丢失，则无法再为已发布的客户端签名新版本。
"@
}

$keyContent = (Get-Content -LiteralPath $KeyFile -Raw).Trim()
if ([string]::IsNullOrWhiteSpace($keyContent)) { throw "私钥文件为空：$KeyFile" }

$keyKind = Get-KeyKind -Base64Payload $keyContent
if ($keyKind -notmatch 'secret key') {
    throw "该文件不是 minisign 私钥（首行：$keyKind）：$KeyFile"
}
Write-Host "    私钥：$KeyFile"

# ── 2) 口令 + keyid 预检（构建前快速失败）───────────────────────────────
Write-Host "==> [2/4] 校验口令与密钥配对" -ForegroundColor Cyan

if (-not (Test-Path -LiteralPath $PasswordFile)) {
    throw @"
未找到私钥口令文件：$PasswordFile

口令必须存在才能非交互构建：tauri 在缺口令时不会报错，而是**阻塞等待键盘输入**，
表现为"构建卡住"。请写入一行口令，或用 -PasswordFile 指定其他位置。
"@
}
$password = (Get-Content -LiteralPath $PasswordFile -Raw).Trim()
if ([string]::IsNullOrWhiteSpace($password)) { throw "口令文件为空：$PasswordFile" }

$confPath = Join-Path $root "src-tauri/tauri.conf.json"
if (-not (Test-Path -LiteralPath $confPath)) { throw "未找到配置文件：$confPath" }
$conf = Get-Content -LiteralPath $confPath -Raw | ConvertFrom-Json
$pubKey = $conf.plugins.updater.pubkey
if ([string]::IsNullOrWhiteSpace($pubKey)) { throw "tauri.conf.json 中缺少 plugins.updater.pubkey" }

$privId = Get-MinisignKeyId -Base64Payload $keyContent
$pubId = Get-MinisignKeyId -Base64Payload $pubKey
Write-Host "    公钥 keyid：$pubId"
Write-Host "    私钥文件标识：$privId（加密私钥的 keynum 与公钥偏移不同，仅供参考，不作判据）"

# 注意：minisign 的加密私钥把 keynum 放在 scrypt 参数之后（偏移 54..62），
# 与公钥（偏移 2..10）不同，因此不能拿私钥文件里的 keynum 直接和公钥比。
# 唯一可靠的验证方式是实际签一次：tauri signer 用私钥解密后产出的签名，
# 其 keyid 必然等于对应公钥的 keyid。口令错误时它会秒级报错，比等完整构建快得多。
# 另：实测 tauri 在**完全缺失**口令时不是报错，而是阻塞等待 stdin —— 所以这里
# 重定向 stdin 并加超时，确保任何情况下都不会永久挂起。
$probeDir = Join-Path ([System.IO.Path]::GetTempPath()) ("wb-signcheck-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $probeDir | Out-Null
$prevKey = $env:TAURI_SIGNING_PRIVATE_KEY
$prevPw = $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD
try {
    Push-Location $probeDir
    Set-Content -LiteralPath (Join-Path $probeDir "probe.txt") -Value "ai-gateway signing probe" -Encoding ascii
    $env:TAURI_SIGNING_PRIVATE_KEY = $keyContent
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $password

    $tauriCli = Join-Path $root "node_modules\.bin\tauri.cmd"
    if (-not (Test-Path -LiteralPath $tauriCli)) {
        throw "未找到 tauri CLI：$tauriCli（请先执行 npm ci）"
    }

    # 用超时保护：万一将来 tauri 改为在别处等待输入，这里也不会永久挂起。
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $tauriCli
    $psi.Arguments = "signer sign probe.txt"
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.RedirectStandardInput = $true      # 不给 stdin 机会：缺口令时立即失败而非挂起
    $psi.UseShellExecute = $false
    $psi.WorkingDirectory = $probeDir
    $proc = [System.Diagnostics.Process]::Start($psi)
    if (-not $proc.WaitForExit(60000)) {
        $proc.Kill($true)
        throw "签名预检超时（60 秒）：tauri signer 未返回，疑似在等待交互输入。"
    }
    $probeOut = $proc.StandardOutput.ReadToEnd() + $proc.StandardError.ReadToEnd()
    if ($proc.ExitCode -ne 0) {
        throw "签名预检失败（口令是否正确？）：`n$probeOut"
    }

    $sigPath = Join-Path $probeDir "probe.txt.sig"
    if (-not (Test-Path -LiteralPath $sigPath)) { throw "签名预检未产出 .sig 文件" }
    $sigId = Get-MinisignKeyId -Base64Payload (Get-Content -LiteralPath $sigPath -Raw)
    if ($sigId -ne $pubId) {
        throw @"
签名 keyid 与配置公钥不配对：签名 $sigId != 公钥 $pubId

这把私钥签出的更新包会被客户端拒绝（"签名验证失败"）。
请改用与 tauri.conf.json 中 pubkey 配对的私钥。
"@
    }
    Write-Host "    签名 keyid：$sigId（与公钥配对）" -ForegroundColor Green
}
finally {
    Pop-Location -ErrorAction SilentlyContinue
    $env:TAURI_SIGNING_PRIVATE_KEY = $prevKey
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $prevPw
    Remove-Item -Recurse -Force $probeDir -ErrorAction SilentlyContinue
}

if ($CheckOnly) {
    Write-Host ""
    Write-Host "==> 预检通过（未执行构建）" -ForegroundColor Green
    return
}

# ── 3) 构建/刷新内嵌的 Go 网关 ─────────────────────────────────────────
#
# # 为什么这一步必须在这里（2026-09-21 所有者反馈，这是结构性缺陷）
#
# 所有者原话：
#
#	「我发现每次都会内嵌的是旧的网关，没办法从构建上，直接写一个脚本
#	  一次性处理掉这个问题吗？」
#
# 根因：`crates/ai-gateway-core/build.rs` 会**无条件内嵌**
# `crates/ai-gateway-core/embedded/gateway.exe` 里当时躺着的那一份，
# 而**本脚本此前从不构建网关**（只有 build-single.ps1 有那段逻辑）。
#
# 于是：改了 `go-gateway/` 的代码但没手动重建 embedded/gateway.exe，
# 打出来的包就内嵌旧网关 —— 症状是"新功能在开发机上好好的，装完却没有"，
# 而构建日志里**没有任何异常**。所有者已经不止一次踩到。
#
# 修法：把构建网关做成打包的**前置步骤**，并在构建后做指纹校验。
# 「记得先手动跑一次」不是修法 —— 那正是会忘的事。
$embeddedGw = Join-Path $root "crates/ai-gateway-core/embedded/gateway.exe"
$gwScript = Join-Path $PSScriptRoot "build-gateway.ps1"

Write-Host "==> [3/4] 构建/刷新内嵌网关（Go）" -ForegroundColor Cyan
if (-not (Test-Path -LiteralPath $gwScript)) {
    throw "未找到 $gwScript —— 内嵌网关无法保证新鲜，拒绝继续构建"
}

# -Force：打包场景下一律重建。
#
# 为什么不用"源码比产物新才重建"的增量判断：Go 的构建很快（约 10 秒），
# 而"内嵌了旧网关"这个错误的代价极高（一个装完才发现功能没生效的安装包）。
# 用 10 秒换"绝不可能内嵌旧网关"，在这个场景下是明显划算的交易。
# （需要快速迭代 UI 时可用 -SkipGateway 跳过，见下方参数。）
if ($SkipGateway) {
    Write-Host "    跳过（-SkipGateway，沿用现有 embedded/gateway.exe）" -ForegroundColor Yellow
    if (-not (Test-Path -LiteralPath $embeddedGw)) {
        throw "-SkipGateway 但 embedded/gateway.exe 不存在：无法内嵌任何网关"
    }
} else {
    & $gwScript -Force | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "构建内嵌网关失败（exit $LASTEXITCODE）" }
}

# 记下**构建前**的指纹：构建后要拿它比对，确认内嵌的确实是这一份。
$gwHashBefore = (Get-FileHash -LiteralPath $embeddedGw -Algorithm SHA256).Hash
$gwSizeBefore = [math]::Round((Get-Item -LiteralPath $embeddedGw).Length / 1MB, 2)
Write-Host ("    待内嵌网关：{0} MB  SHA256 {1}…" -f $gwSizeBefore, $gwHashBefore.Substring(0, 16)) -ForegroundColor Green

# ── 4) 构建 ────────────────────────────────────────────────────────────
Write-Host "==> [4/4] 构建签名安装包（bundles=$Bundles）" -ForegroundColor Cyan
Push-Location $root
try {
    if (-not (Test-Path (Join-Path $root "node_modules"))) {
        Write-Host "    未找到 node_modules，先执行 npm ci"
        npm ci
        if ($LASTEXITCODE -ne 0) { throw "npm ci 失败" }
    }

    $env:TAURI_SIGNING_PRIVATE_KEY = $keyContent
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $password
    # ⚠ npm 会把**普通的进度信息**写到 stderr（如 "Info Looking up installed
    # tauri packages…"）。而 PowerShell 5.1 在 `$ErrorActionPreference = 'Stop'`
    # 下会把原生命令的 stderr 当成**致命错误**并**中止整个脚本** ——
    # 于是后面的"复制到 dist"根本不执行，但产物其实**已经构建成功**。
    #
    # 症状：脚本报 exit 1，而 `target/release/bundle/nsis/` 里躺着完好的安装包。
    # 我据此误判过两次"打包失败"。
    #
    # 修法：这一段临时把 stderr 当普通输出流（`2>&1`），并**只**用
    # `$LASTEXITCODE` 判断成败 —— 那才是原生命令真实的结果。
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        npm run tauri -- build --bundles $Bundles 2>&1 |
            ForEach-Object { Write-Host "    $_" }
    } finally {
        $ErrorActionPreference = $prevEAP
    }
    if ($LASTEXITCODE -ne 0) { throw "tauri build 失败（exit $LASTEXITCODE）" }
}
finally {
    Pop-Location
    $env:TAURI_SIGNING_PRIVATE_KEY = $prevKey
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $prevPw
}

# ── 校验 A：内嵌的确实是刚构建的那份网关 ───────────────────────────────
#
# # 这一步是本次修复的核心（2026-09-21）
#
# 光"构建前重建网关"还不够 —— 那只能保证**磁盘上**的 embedded/gateway.exe
# 是新的，不能保证**打进包里**的是它。中间还隔着 build.rs 的查找顺序：
#
#	1. 环境变量 AI_GATEWAY_ROUTER_BIN
#	2. crates/ai-gateway-core/embedded/gateway[.exe]
#	3. 仓库根 dist/gateway[.exe]
#
# 若环境变量或 dist/ 下躺着一份**更早**的网关，build.rs 会优先内嵌它，
# 而构建日志只有一行 `cargo:warning=已内嵌网关: <路径>`（极易忽略）。
#
# 故这里做**双端指纹比对**：
#   · 构建后重新哈希 embedded/gateway.exe —— 必须与构建前一致
#     （若中途被别的进程改过，说明有并发构建，产物不可信）
#   · 再确认没有"更高优先级"的候选路径在截胡
#
# 这是"内嵌旧网关"在物理上无法交付的最后一道闸。
Write-Host "==> 校验内嵌网关指纹" -ForegroundColor Cyan

$gwHashAfter = (Get-FileHash -LiteralPath $embeddedGw -Algorithm SHA256).Hash
if ($gwHashAfter -ne $gwHashBefore) {
    throw @"
内嵌网关在构建期间被改动，产物不可信。

  构建前：$gwHashBefore
  构建后：$gwHashAfter

可能原因：有另一个构建/脚本正在并发写 $embeddedGw。
请确认没有并发构建后重试。
"@
}
Write-Host ("    embedded/gateway.exe 未被改动  [OK]") -ForegroundColor Green

# 检查有没有"更高优先级"的候选在截胡 build.rs 的选择。
#
# 只报告**存在的**候选：build.rs 会按顺序取第一个 >1MB 的文件。
# 若 dist/gateway.exe 存在且比 embedded 的旧，那它**不会**被选中
#（embedded 在它前面），故只需在它比 embedded 新时才警告 ——
# 那种情况下 build.rs 仍然选 embedded，所以其实无害，但值得提示。
$routerBin = $env:AI_GATEWAY_ROUTER_BIN
if ($routerBin -and (Test-Path -LiteralPath $routerBin)) {
    $rh = (Get-FileHash -LiteralPath $routerBin -Algorithm SHA256).Hash
    if ($rh -ne $gwHashBefore) {
        throw @"
检测到 AI_GATEWAY_ROUTER_BIN 指向**另一份**网关，它会覆盖 embedded/ 的选择。

  AI_GATEWAY_ROUTER_BIN = $routerBin
  它的 SHA256          = $rh
  embedded 的 SHA256   = $gwHashBefore

build.rs 的查找顺序里环境变量**优先于** embedded/，故本次包内嵌的是它，
而不是你刚构建的那份。请 unset 该变量或让它指向同一份产物。
"@
    }
    Write-Host "    AI_GATEWAY_ROUTER_BIN 与 embedded 一致  [OK]" -ForegroundColor Green
}

$distGw = Join-Path $root "dist/gateway.exe"
if (Test-Path -LiteralPath $distGw) {
    $dh = (Get-FileHash -LiteralPath $distGw -Algorithm SHA256).Hash
    if ($dh -eq $gwHashBefore) {
        Write-Host "    dist/gateway.exe 与 embedded 一致（无影响）" -ForegroundColor Green
    } else {
        # embedded 在查找顺序里**先于** dist/，故 build.rs 选的是 embedded。
        # 这里只提示，不失败 —— 但要让用户知道 dist/ 里躺着一份不同的网关，
        # 因为它是个容易被误认为"生效了"的陷阱。
        Write-Host ("    提示：dist/gateway.exe 与本次内嵌的不同（SHA256 {0}…）。" -f $dh.Substring(0, 16)) -ForegroundColor Yellow
        Write-Host ("          build.rs 的查找顺序里 embedded/ 优先，故本次内嵌的仍是 embedded 那份。" ) -ForegroundColor Yellow
        Write-Host ("          若你期望的是 dist/ 那份，请设 AI_GATEWAY_ROUTER_BIN 指向它。" ) -ForegroundColor Yellow
    }
}

# ── 3.5) 校验内嵌前端是**当次构建**的那份（2026-09-21）────────────────
#
# # 为什么需要它（真实事故）
#
# 所有者报告：
#
#	「zcode 这里还是老卡片啊 你是不是没打包？」
#
# 查证结果：**代码与安装包都是对的，但他装的是更早的那一版**。
# 而我在排查时用了一个**不可靠的判据**（在 exe 里搜前端代码里的字符串），
# 得出"新渲染没进包"的错误结论，白绕了一大圈 —— 因为 Tauri 对 JS
# **内容**做了压缩，那些字符串在 exe 里根本搜不到
#（连早就存在的老标识 `product-usage-bar` 也搜不到）。
#
# 可靠且简单的判据是**资源文件名**：
#
#	· `dist/index.html` 里写死了它引用哪个 `assets/index-XXXX.js`
#	· 那个名字是 Vite 按**内容哈希**生成的 —— 内容变则名字变
#	· 文件名在 exe 里是**明文**（Tauri 只压 JS 内容，不压文件名）
#
# 于是比对「exe 里出现的 index-*.js 名字」与「dist/index.html 引用的名字」
# 就能确定内嵌的是不是当前前端 —— 一次字符串查找，无需启动 GUI。
#
# ⚠ 为什么这一步是必要的：`tauri-build` 的 build script 里
# `rerun-if-changed` **不包含** `../dist`，故前端重建**不会**触发
# 重新嵌入 —— cargo 可能沿用上一次编进二进制的旧前端。
# 这与上面"内嵌旧网关"是**同型**缺陷，只是换成了前端。
Write-Host "==> 校验内嵌前端指纹" -ForegroundColor Cyan

$distIndex = Join-Path $root "dist/index.html"
if (-not (Test-Path -LiteralPath $distIndex)) {
    throw "找不到 $distIndex —— 前端没构建，无法校验内嵌资源"
}

$distHtml = Get-Content -LiteralPath $distIndex -Raw
$assetMatch = [regex]::Match($distHtml, 'assets/index-[A-Za-z0-9_\-]+\.js')
if (-not $assetMatch.Success) {
    throw "无法从 $distIndex 解析出入口 JS 文件名（Vite 产物格式变了？）"
}
$wantAsset = $assetMatch.Value
Write-Host "    dist 入口：$wantAsset"

$appExe = Join-Path $root "target/release/ai-gateway.exe"
if (-not (Test-Path -LiteralPath $appExe)) {
    throw "找不到 $appExe —— 打包产物缺失，无法校验内嵌前端"
}
$appText = [System.Text.Encoding]::ASCII.GetString(
    [System.IO.File]::ReadAllBytes($appExe))
$embeddedAssets = @(
    [regex]::Matches($appText, 'assets/index-[A-Za-z0-9_\-]+\.js') |
        ForEach-Object { $_.Value } | Sort-Object -Unique
)

if ($embeddedAssets -notcontains $wantAsset) {
    throw @"
内嵌前端不是当次构建的那份，产物不可信。

  dist 引用：$wantAsset
  exe 内嵌 ：$($embeddedAssets -join ', ')

可能原因：
  · tauri-build 的 build script 没有重跑 —— 它的 rerun-if-changed 列表里
    **没有** ../dist，故前端重建**不会**触发重新嵌入
  · 有并发构建在写 target/release

修法（按顺序试）：
  1) cargo clean -p ai-gateway    然后重跑本脚本
  2) 仍不行：删 target/release/ai-gateway.exe 再构建
  3) 再不行：cargo clean 全量重建（慢，但一定干净）
"@
}
Write-Host "    内嵌前端与 dist 一致：$wantAsset  [OK]" -ForegroundColor Green

# ── 4) 校验产物与签名 ──────────────────────────────────────────────────
Write-Host "==> 校验产物签名" -ForegroundColor Cyan
$bundleRoot = Join-Path $root "target/release/bundle"
if (-not (Test-Path -LiteralPath $bundleRoot)) {
    $bundleRoot = Join-Path $root "src-tauri/target/release/bundle"
}
$sigs = @(Get-ChildItem -Path $bundleRoot -Recurse -File -Filter *.sig -ErrorAction SilentlyContinue)
if ($sigs.Count -eq 0) {
    Write-Warning "未在 $bundleRoot 下找到任何 .sig —— 请确认 createUpdaterArtifacts 与 bundles 设置"
} else {
    foreach ($s in $sigs) {
        $id = Get-MinisignKeyId -Base64Payload (Get-Content -LiteralPath $s.FullName -Raw)
        $mark = if ($id -eq $pubId) { "OK" } else { "不匹配" }
        Write-Host ("    {0}  [{1}]" -f $s.FullName, $mark)
    }
}

# ── 5) 把产物复制到**固定的** dist/ ────────────────────────────────────
#
# # 为什么必须有这一步（2026-09-20 补，所有者明确要求）
#
# 此前脚本只把产物留在 `target/release/bundle/nsis/`（那是构建的中间位置，
# 混在成千上万个 .rlib/.pdb 里，且会被 `cargo clean` 清掉）。
# 于是每次交付都要**手动**去找包、手动决定放哪 —— 结果就是我每次
# 随手新建一个目录（`dist-1.0.7`、`release-v0.7.1` …）。
#
# 所有者原话：
#
#	「你为什么每次打包都换一个新的目录？产物应该保持统一的位置，
#	  而不是每次都改变」
#
# **根因是脚本没有固定出口**，而不是我"记性不好" —— 故修在脚本里：
# 构建一结束就自动落到 `dist/`，这样"放哪"不再是每次要做的决定。
#
# ⚠ `dist/` 里还有历史草稿（HTML 草稿、诊断 md），故**只复制不清理**。
#
# ⚠ **不能写成 `Join-Path $root ".." "dist"`** —— PowerShell 5.1 的
# `Join-Path` 只接受**两个**位置参数（第三个会报
# "找不到接受实际参数 dist 的位置形式参数"）。先拼一层再拼一层。
$distDir = Join-Path (Split-Path $root -Parent) "dist"
$distDir = [System.IO.Path]::GetFullPath($distDir)
if (-not (Test-Path -LiteralPath $distDir)) {
    New-Item -ItemType Directory -Path $distDir -Force | Out-Null
}
Write-Host ""
Write-Host "==> 复制产物到 dist/" -ForegroundColor Cyan
$delivered = @()
# 只搬"安装包 + 它的 .sig"，不搬整个 bundle 树（那里面有大量中间产物）
foreach ($s in $sigs) {
    $installer = $s.FullName -replace '\.sig$', ''
    if (-not (Test-Path -LiteralPath $installer)) { continue }
    foreach ($f in @($installer, $s.FullName)) {
        $target = Join-Path $distDir (Split-Path $f -Leaf)
        Copy-Item -LiteralPath $f -Destination $target -Force
        $delivered += $target
    }
}
if ($delivered.Count -eq 0) {
    Write-Warning "没有可交付的产物（未找到与 .sig 配对的安装包）"
} else {
    foreach ($f in $delivered) {
        $len = [math]::Round((Get-Item -LiteralPath $f).Length / 1MB, 2)
        Write-Host ("    {0}  ({1} MB)" -f $f, $len)
    }
    # 复制后校验：哈希不一致说明写盘出错，宁可报出来也不要交付坏包
    foreach ($s in $sigs) {
        $installer = $s.FullName -replace '\.sig$', ''
        if (-not (Test-Path -LiteralPath $installer)) { continue }
        $copied = Join-Path $distDir (Split-Path $installer -Leaf)
        if (-not (Test-Path -LiteralPath $copied)) { continue }
        $h1 = (Get-FileHash -LiteralPath $installer -Algorithm SHA256).Hash
        $h2 = (Get-FileHash -LiteralPath $copied -Algorithm SHA256).Hash
        if ($h1 -eq $h2) {
            Write-Host ("    [OK] SHA256 一致  {0}" -f (Split-Path $copied -Leaf)) -ForegroundColor Green
        } else {
            Write-Warning ("SHA256 不一致，副本可能损坏：{0}" -f $copied)
        }
    }
}

Write-Host ""
Write-Host "==> 完成" -ForegroundColor Green
Write-Host "    产物目录：$bundleRoot（构建原生产物）"
Write-Host "    交付目录：$distDir（固定位置，安装包 + .sig 已就位）"
Write-Host "    发布：把 *_setup.exe 与其 .sig、latest.json 一并上传到 GitHub Release"
Write-Host "          （现有 scripts/publish-release.sh 覆盖此流程，但它依赖 bash/python3/gh）"

