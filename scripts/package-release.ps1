<#
.SYNOPSIS
    打包发布产物到**固定路径** release-out/，并清掉其它版本的旧安装包。

.DESCRIPTION
    为什么需要这个脚本：

    在此之前，安装包的落点是零散的：dist-bin/、target/release/bundle/nsis/、
    以及更名那一代的 _rel/。同一台机器上会同时躺着多个版本的 setup.exe，
    "哪个才是最该发布的那份"每次都要靠翻时间戳判断，误发旧包的风险实打实存在
    （发错版本会把用户降级）。

    本脚本把这件事收敛成一条规矩：

        **发行产物只有 release-out/ 一个落点，且只保留当前版本。**

    具体做三件事：

      1. 从 tauri 构建目录取来当前版本的安装包与 .sig，按**发布固定名**重命名
         （ai-gateway-windows-x86_64-setup.exe）。不用原始名，是因为 tauri updater
         清单里的 signature 是对"下载 URL 对应的文件名"签的，而 Release 上为了
         固定 URL 用的是这个固定名 —— 名字不同则签名校验必失败。
         因此**签名必须对重命名后的文件重做**，不能直接搬 tauri 生成的 .sig。
      2. 生成 latest.json（updater 清单），并校验签名 keyid 与 tauri.conf.json
         的公钥配对 —— 不配对就终止，绝不产出一份客户端会拒绝的清单。
      3. 删除 release-out/ 下**其它版本**的安装包（保留当前版本），
         并按需清理仓库内历史遗留的旧安装包目录。

    私钥与口令的固定位置见 scripts/build-signed.ps1（仓库外，零提交风险）：
      %USERPROFILE%\.wb-switch\wb-switch-updater.key
      %USERPROFILE%\.wb-switch\wb-switch-updater.password

.PARAMETER Version
    要打包的版本号（x.y.z）。缺省从 src-tauri/tauri.conf.json 读，与构建产物一致。

.PARAMETER SkipBuild
    跳过构建，直接复用 target/release/bundle 下已存在的当前版本产物。
    适合"构建已完成、只想重新落盘/重签名"的场景。

.PARAMETER KeepOthers
    不在 release-out/ 里清理其它版本的安装包（默认清理）。

.PARAMETER PruneLegacyDirs
    额外清理仓库内历史遗留的旧安装包目录（_rel/、dist-bin/ 下的 setup.exe）。
    这是一次性的仓库整理动作，因此做成显式开关而不是默认行为。

.EXAMPLE
    pwsh scripts/package-release.ps1
    构建并打包当前版本，产物落到 release-out/，清掉该目录下其它版本。

.EXAMPLE
    pwsh scripts/package-release.ps1 -SkipBuild
    复用已有构建产物，只重新落盘 + 重签名 + 生成 latest.json。

.EXAMPLE
    pwsh scripts/package-release.ps1 -PruneLegacyDirs
    打包之外，顺手清掉仓库里历史遗留的旧安装包目录。
#>
[CmdletBinding()]
param(
    [string]$Version = "",
    [switch]$SkipBuild,
    [switch]$KeepOthers,
    [switch]$PruneLegacyDirs
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot   # 仓库根
$outDirName = "release-out"
$outDir = Join-Path $root $outDirName

# 发布固定名：与 tauri.conf.json 的 updater endpoint、以及
# scripts/publish-release.sh 的 SETUP_NAME 必须完全一致。
# 写错会让自动更新 404。
$setupName = "ai-gateway-windows-x86_64-setup.exe"

# ── 工具函数（与 build-signed.ps1 同源：从 base64 载荷取 minisign keyid）──
#
# 私钥、公钥、签名三者格式一致，因此可用同一函数比对 keyid ——
# 这是判断"这份签名能否被配置公钥验证"的唯一可靠依据。
function Get-MinisignKeyId {
    param([Parameter(Mandatory)][string]$Base64Payload)
    $outer = [System.Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($Base64Payload.Trim()))
    $lines = $outer.Trim() -split "`n"
    if ($lines.Count -lt 2) { throw "载荷格式异常：解出后不足两行" }
    $inner = [Convert]::FromBase64String($lines[1].Trim())
    if ($inner.Length -lt 10) { throw "载荷格式异常：内层不足 10 字节" }
    return (($inner[2..9] | ForEach-Object { $_.ToString('x2') }) -join '')
}

# ── 1) 解析版本 ─────────────────────────────────────────────────────────
Write-Host "==> [1/5] 解析版本" -ForegroundColor Cyan

$confPath = Join-Path $root "src-tauri/tauri.conf.json"
if (-not (Test-Path -LiteralPath $confPath)) { throw "未找到配置文件：$confPath" }
$conf = Get-Content -LiteralPath $confPath -Raw | ConvertFrom-Json

if (-not $Version) { $Version = $conf.version }
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "版本号格式应为 x.y.z，收到：$Version"
}
# 版本必须与配置一致，否则会打出一个"文件名写 A、内容其实是 B"的包。
if ($Version -ne $conf.version) {
    throw @"
版本号不一致：参数给定 $Version，但 tauri.conf.json 是 $($conf.version)。

请先执行 `pwsh scripts/bump-version.ps1 <版本>` 统一版本号，
再打包 —— 否则产物名与二进制内声明的版本会对不上。
"@
}
Write-Host "    版本：$Version"

$pubKey = $conf.plugins.updater.pubkey
if ([string]::IsNullOrWhiteSpace($pubKey)) { throw "tauri.conf.json 中缺少 plugins.updater.pubkey" }
$pubId = Get-MinisignKeyId -Base64Payload $pubKey
Write-Host "    公钥 keyid：$pubId"

# ── 2) 构建（或复用）────────────────────────────────────────────────────
Write-Host "==> [2/5] 构建签名安装包" -ForegroundColor Cyan

$bundleDir = Join-Path $root "target/release/bundle/nsis"
if (-not (Test-Path -LiteralPath $bundleDir)) {
    $alt = Join-Path $root "src-tauri/target/release/bundle/nsis"
    if (Test-Path -LiteralPath $alt) { $bundleDir = $alt }
}
$builtExe = Join-Path $bundleDir "AI Gateway_${Version}_x64-setup.exe"

if ($SkipBuild -and (Test-Path -LiteralPath $builtExe)) {
    Write-Host "    跳过构建（-SkipBuild，复用 $builtExe）"
} else {
    # 顺序至关重要：**先重建内嵌网关，再跑 tauri build**。
    #
    # tauri build 只是把 crates/ai-gateway-core/embedded/gateway.exe 原样压进主程序，
    # 它**不会**重新编译网关。若跳过这一步，改过 crates/ai-gateway-router 的代码
    # 就打不进安装包 —— 产物看起来是新的（版本号、时间戳都新），网关却还是旧的，
    # 表现为"修好的 bug 在装好的客户端里依然存在"。这个坑实测踩过。
    Write-Host "    [a] 重建内嵌网关（crates/ai-gateway-router → embedded/gateway.exe）"
    & (Join-Path $PSScriptRoot "build-gateway.ps1")
    if ($LASTEXITCODE -ne 0) { throw "网关构建失败（退出码 $LASTEXITCODE）" }

    Write-Host "    [b] 构建签名安装包（含密钥/口令预检）"
    & (Join-Path $PSScriptRoot "build-signed.ps1") -Bundles nsis
    if ($LASTEXITCODE -ne 0) { throw "构建失败（退出码 $LASTEXITCODE）" }
}

if (-not (Test-Path -LiteralPath $builtExe)) {
    throw @"
未找到当前版本的构建产物：$builtExe

请先构建（去掉 -SkipBuild），或确认版本号 $Version 与 tauri.conf.json 一致。
"@
}

# 内嵌网关的新鲜度校验：embedded/gateway.exe 必须**不早于**它源码的最后修改时间。
#
# 这是上面那个坑的守门人：tauri build 不会重编网关，若某次跳过重建就打包，
# 产物会带着旧网关 —— 而版本号、安装包时间戳全是新的，肉眼完全看不出。
# 用 mtime 比较能让这种"静默用了旧网关"在打包阶段就报错，而不是等用户装上才发现。
$embeddedExe = Join-Path $root "crates/ai-gateway-core/embedded/gateway.exe"
if (-not (Test-Path -LiteralPath $embeddedExe)) {
    throw "未找到内嵌网关：$embeddedExe（请先执行 scripts/build-gateway.ps1）"
}
$embeddedTime = (Get-Item -LiteralPath $embeddedExe).LastWriteTimeUtc
# 网关源码在 router crate 与 core crate 两处（core 也含网关模块）。
$srcRoots = @(
    (Join-Path $root "crates/ai-gateway-router/src"),
    (Join-Path $root "crates/ai-gateway-core/src")
)
$newestSrc = $null
foreach ($sr in $srcRoots) {
    if (-not (Test-Path -LiteralPath $sr)) { continue }
    Get-ChildItem -LiteralPath $sr -Recurse -File -Include *.rs -ErrorAction SilentlyContinue |
        ForEach-Object {
            if ($null -eq $newestSrc -or $_.LastWriteTimeUtc -gt $newestSrc) { $newestSrc = $_.LastWriteTimeUtc }
        }
}
if ($null -ne $newestSrc -and $newestSrc -gt $embeddedTime) {
    throw @"
内嵌网关比网关源码旧，打出来的安装包会带着**旧网关**。

  内嵌网关：$embeddedExe
             $($embeddedTime.ToString('yyyy-MM-dd HH:mm:ss')) UTC
  最新源码：$($newestSrc.ToString('yyyy-MM-dd HH:mm:ss')) UTC

这是最隐蔽的一类错：tauri build 不重编网关，只是把它原样压进主程序，
因此版本号与安装包时间戳都是新的，装出来的客户端里跑的却还是旧代码。

请去掉 -SkipBuild 重新打包（本脚本会先跑 build-gateway.ps1 重建网关）。
"@
}
Write-Host ("    内嵌网关：{0} UTC（新于源码 {1} UTC，通过新鲜度校验）" -f `
    $embeddedTime.ToString('MM-dd HH:mm'), $newestSrc.ToString('MM-dd HH:mm')) -ForegroundColor Green

$sizeMB = (Get-Item -LiteralPath $builtExe).Length / 1MB
Write-Host ("    产物：{0}（{1:N2} MB）" -f (Split-Path -Leaf $builtExe), $sizeMB)

# ── 3) 落盘到固定路径 + 用发布固定名重签名 ──────────────────────────────
Write-Host "==> [3/5] 落盘到 $outDirName/ 并重签名" -ForegroundColor Cyan

New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$destExe = Join-Path $outDir $setupName
$destSig = "$destExe.sig"

# 必须先复制再签名：tauri 生成的 .sig 是对"原始文件名"签的，
# 而清单里的 signature 要对应"下载 URL 的文件名"。名字一变，签名即失效。
Copy-Item -LiteralPath $builtExe -Destination $destExe -Force

$keyFile = Join-Path $env:USERPROFILE ".wb-switch\wb-switch-updater.key"
$pwFile = Join-Path $env:USERPROFILE ".wb-switch\wb-switch-updater.password"
if (-not (Test-Path -LiteralPath $keyFile)) {
    $legacyKey = Join-Path $env:USERPROFILE ".ai-gateway\wb-switch-updater.key"
    if (Test-Path -LiteralPath $legacyKey) { $keyFile = $legacyKey }
}
if (-not (Test-Path -LiteralPath $pwFile)) {
    $legacyPw = Join-Path $env:USERPROFILE ".ai-gateway\wb-switch-updater.password"
    if (Test-Path -LiteralPath $legacyPw) { $pwFile = $legacyPw }
}
if (-not (Test-Path -LiteralPath $keyFile)) { throw "未找到签名私钥：$keyFile" }
if (-not (Test-Path -LiteralPath $pwFile)) { throw "未找到私钥口令：$pwFile" }

$prevKey = $env:TAURI_SIGNING_PRIVATE_KEY
$prevPw = $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD
try {
    $env:TAURI_SIGNING_PRIVATE_KEY = (Get-Content -LiteralPath $keyFile -Raw).Trim()
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = (Get-Content -LiteralPath $pwFile -Raw).Trim()
    $tauriCli = Join-Path $root "node_modules\.bin\tauri.cmd"
    if (-not (Test-Path -LiteralPath $tauriCli)) { throw "未找到 tauri CLI：$tauriCli（请先 npm ci）" }
    Push-Location $outDir
    try {
        & $tauriCli signer sign $setupName
        if ($LASTEXITCODE -ne 0) { throw "签名失败（退出码 $LASTEXITCODE）" }
    }
    finally { Pop-Location }
}
finally {
    $env:TAURI_SIGNING_PRIVATE_KEY = $prevKey
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = $prevPw
}

if (-not (Test-Path -LiteralPath $destSig)) { throw "签名未产出 .sig：$destSig" }

# ── 4) 生成 latest.json 并校验签名配对 ──────────────────────────────────
Write-Host "==> [4/5] 生成 latest.json 并校验签名" -ForegroundColor Cyan

$sig = (Get-Content -LiteralPath $destSig -Raw).Trim()
$url = "https://github.com/momo0410/ai-gateway/releases/latest/download/$setupName"
$pubDate = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")

# 签名 keyid 必须等于配置公钥 keyid，否则客户端一律拒绝更新。
$sigId = Get-MinisignKeyId -Base64Payload $sig
if ($sigId -ne $pubId) {
    Remove-Item -LiteralPath $destExe, $destSig, (Join-Path $outDir "latest.json") -ErrorAction SilentlyContinue
    throw @"
签名 keyid 与配置公钥不配对：签名 $sigId != 公钥 $pubId

这份清单会被客户端拒绝（"签名验证失败"），因此不产出。
请确认用的是与 tauri.conf.json pubkey 配对的那把私钥。
"@
}

$manifest = [ordered]@{
    version   = $Version
    notes     = ""
    pub_date  = $pubDate
    platforms = [ordered]@{
        # nsis 是 Tauri 2 的 windows target 键；裸键兼容旧客户端读取逻辑。
        "windows-x86_64-nsis" = [ordered]@{ signature = $sig; url = $url }
        "windows-x86_64"      = [ordered]@{ signature = $sig; url = $url }
    }
}
$manifestPath = Join-Path $outDir "latest.json"
$json = $manifest | ConvertTo-Json -Depth 8
# 无 BOM 写入：带 BOM 的 JSON 会让部分解析器报错（tauri updater 容忍，
# 但保持与其他生成脚本一致更省心）。
[System.IO.File]::WriteAllText($manifestPath, $json + "`n", (New-Object System.Text.UTF8Encoding($false)))
Write-Host "    签名 keyid：$sigId（与公钥配对）" -ForegroundColor Green
Write-Host "    已生成：$manifestPath"

# ── 5) 清理其它版本 ─────────────────────────────────────────────────────
Write-Host "==> [5/5] 清理其它版本" -ForegroundColor Cyan

if ($KeepOthers) {
    Write-Host "    跳过（-KeepOthers）"
} else {
    # 只删 release-out/ 下**不是当前版本**的安装包与清单。
    # 判据用「文件名里是否含当前版本号」，因此当前版本的三件套一定留得住。
    $removed = 0
    Get-ChildItem -LiteralPath $outDir -File | Where-Object {
        $_.Name -match '_x64-setup\.exe(\.sig)?$' -and $_.Name -notmatch [regex]::Escape($Version)
    } | ForEach-Object {
        Write-Host ("    删除旧版本：{0}" -f $_.Name) -ForegroundColor Yellow
        Remove-Item -LiteralPath $_.FullName -Force
        $removed++
    }
    if ($removed -eq 0) { Write-Host "    无需清理" }
}

if ($PruneLegacyDirs) {
    # 历史遗留目录里的一次性清理：这些目录是更名/换实现那几代留下的，
    # 以后不再往里写东西，因此仅在显式开启时清掉。
    foreach ($legacy in @("_rel", "dist-bin")) {
        $p = Join-Path $root $legacy
        if (-not (Test-Path -LiteralPath $p)) { continue }
        Get-ChildItem -LiteralPath $p -File -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -match '\.exe(\.sig)?$' -or $_.Name -match '^latest.*\.json$' } |
            ForEach-Object {
                Write-Host ("    删除遗留产物：{0}/{1}" -f $legacy, $_.Name) -ForegroundColor Yellow
                Remove-Item -LiteralPath $_.FullName -Force
            }
    }
}

# ── 汇总 ────────────────────────────────────────────────────────────────
Write-Host ""
Write-Host "==> 完成" -ForegroundColor Green
Write-Host "    输出目录：$outDir"
Get-ChildItem -LiteralPath $outDir -File | ForEach-Object {
    Write-Host ("      {0}  ({1:N2} MB)" -f $_.Name, ($_.Length / 1MB))
}
Write-Host ""
Write-Host "    发布：gh release upload <tag> $outDirName/* --clobber"
Write-Host "          （scripts/publish-release.sh 覆盖同一流程，但它依赖 bash/python3）"
