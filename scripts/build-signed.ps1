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
      %USERPROFILE%\.ai-gateway\ai-gateway-updater.key       minisign 私钥
      %USERPROFILE%\.ai-gateway\ai-gateway-updater.password  私钥口令

.PARAMETER Bundles
    要构建的 bundle 类型，默认 "nsis"（Windows 安装包）。多平台用逗号分隔，如 "nsis,msi"。

.PARAMETER KeyFile
    私钥文件路径，默认 %USERPROFILE%\.ai-gateway\ai-gateway-updater.key。

.PARAMETER PasswordFile
    口令文件路径，默认 %USERPROFILE%\.ai-gateway\ai-gateway-updater.password。

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
    [string]$KeyFile = (Join-Path $env:USERPROFILE ".ai-gateway\ai-gateway-updater.key"),
    [string]$PasswordFile = (Join-Path $env:USERPROFILE ".ai-gateway\ai-gateway-updater.password"),
    [switch]$CheckOnly
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot   # 仓库根

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
Write-Host "==> [1/3] 检查签名私钥" -ForegroundColor Cyan

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
Write-Host "==> [2/3] 校验口令与密钥配对" -ForegroundColor Cyan

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

# ── 3) 构建 ────────────────────────────────────────────────────────────
Write-Host "==> [3/3] 构建签名安装包（bundles=$Bundles）" -ForegroundColor Cyan
Push-Location $root
try {
    if (-not (Test-Path (Join-Path $root "node_modules"))) {
        Write-Host "    未找到 node_modules，先执行 npm ci"
        npm ci
        if ($LASTEXITCODE -ne 0) { throw "npm ci 失败" }
    }

    $env:TAURI_SIGNING_PRIVATE_KEY = $keyContent
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $password
    npm run tauri -- build --bundles $Bundles
    if ($LASTEXITCODE -ne 0) { throw "tauri build 失败" }
}
finally {
    Pop-Location
    $env:TAURI_SIGNING_PRIVATE_KEY = $prevKey
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $prevPw
}

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

Write-Host ""
Write-Host "==> 完成" -ForegroundColor Green
Write-Host "    产物目录：$bundleRoot"
Write-Host "    发布：把 *_setup.exe 与其 .sig、latest.json 一并上传到 GitHub Release"
Write-Host "          （现有 scripts/publish-release.sh 覆盖此流程，但它依赖 bash/python3/gh）"
