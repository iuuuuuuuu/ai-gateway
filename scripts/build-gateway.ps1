# build-gateway.ps1 —— 构建内嵌网关（**Rust**）并放入 embedded/，供 Tauri 内嵌。
#
# # 为什么必须有这个脚本（2026-09-21 所有者反馈）
#
# 所有者的原话：
#
#	「我发现每次都会内嵌的是旧的网关，没办法从构建上，直接写一个脚本
#	  一次性处理掉这个问题吗？」
#
# 他说得对，而且这是**结构性缺陷**，不是偶发：
#
#	scripts/build-single.ps1  → 有「源码比产物新就重建」的逻辑 ✓
#	scripts/build-signed.ps1  → **完全没有构建网关的步骤** ✗
#
# 而 `crates/ai-gateway-core/build.rs` 会**无条件内嵌**
# `crates/ai-gateway-core/embedded/gateway.exe` 里当时躺着的那一份。
# 于是：谁改了网关代码、但没手动重建 embedded/gateway.exe，
# **打出来的安装包就内嵌旧网关** —— 症状是"新功能在开发机上好好的，
# 装完却没有"，而构建日志里没有任何异常。
#
# 修法不是"记得先手动跑一次构建"（那正是会忘的事），而是：
#
#	1. 本脚本把「构建网关」变成**一条命令**；
#	2. build-signed.ps1 在构建前**自动调用**它（见那里的 [3/4] 步）；
#	3. 加**指纹校验**：构建前后比对 embedded 网关的 SHA256，
#	   对不上就**直接失败**，让"内嵌旧网关"在物理上无法交付。
#
# # 为什么从 Go 换成 Rust（2026-10-01，所有者决策）
#
# 所有者原话：
#
#	「我们也换成rust全重写」
#
# 网关内核已完整移植到 `crates/ai-gateway-router`（Rust），与 Go 版逐行对齐、
# 实机自测通过。本脚本随之切换：**不再需要 Go 工具链**。
#
# Go 源码 `go-gateway/` 仍然保留在仓库里，作为移植的对照参考与回归依据
#（Rust 侧的注释大量逐字引用 `internal/**` 的行号），不是死代码。
#
# # 产物
#
#	crates/ai-gateway-core/embedded/gateway.exe    （被 build.rs 内嵌）
#	以及同目录的 gateway.exe.sha256                 （指纹，供校验）
#
# # 用法
#
#	pwsh scripts/build-gateway.ps1              # 增量构建（源码没变则跳过）
#	pwsh scripts/build-gateway.ps1 -Force       # 强制重建
#	pwsh scripts/build-gateway.ps1 -CheckOnly   # 只校验现有产物的新鲜度

[CmdletBinding()]
param(
    # 强制重建，忽略"源码比产物新"的判断。
    [switch]$Force,
    # 只检查产物是否存在、是否比源码新，不执行构建。
    [switch]$CheckOnly,
    # 静默模式：只输出一行结果（供 build-signed.ps1 调用时保持日志整洁）。
    [switch]$Quiet,
    # debug 构建（仅排障用；产物约 22 MB，会显著撑大安装包）。
    #
    # ⚠ 名字**不能**叫 `-Debug`：`[CmdletBinding()]` 会自动注入一批通用参数
    #（Verbose / Debug / ErrorAction …），重名会让**整个脚本**在任何调用下都
    # 直接报 `A parameter with the name 'Debug' was defined multiple times`，
    # 连 `-CheckOnly` 都跑不起来 —— 实测踩过。
    [switch]$DebugBuild,
    # 额外传给 cargo 的参数，如 "--offline"。受限网络下必用。
    [string]$CargoArgs = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot          # 仓库根
$crateDir = Join-Path $root "crates/ai-gateway-router"
$embeddedDir = Join-Path $root "crates/ai-gateway-core/embedded"
$gwExe = Join-Path $embeddedDir "gateway.exe"
$gwSha = Join-Path $embeddedDir "gateway.exe.sha256"
$profile = if ($DebugBuild) { "debug" } else { "release" }

function Say([string]$msg, [string]$color = "Gray") {
    if (-not $Quiet) { Write-Host $msg -ForegroundColor $color }
}

# ── 源码指纹：网关 crate 下所有 .rs / Cargo.toml 的最新 mtime ────────────
#
# 为什么用 mtime 而不是内容哈希：
#   · 构建脚本要快，哈希几千个文件不值得；
#   · 哈希也挡不住"改了文件但 mtime 没变"这种需要手动 touch 的极罕见情况。
# mtime 的假阳性（touch 过但内容没变）代价只是一次多余的重建。
#
# ★ 为什么必须把 workspace 的 Cargo.toml / Cargo.lock 也算进来：
#   Rust 侧依赖来自 workspace 清单与锁文件。只盯 `src/**/*.rs` 的话，
#   「升级了 reqwest 版本但没碰源码」会被判成"没变"而跳过重建 ——
#   而那恰恰是需要重建的一种改动。旧版 Go 把 go.mod/go.sum 一并纳入，
#   这里保持同口径。
function Get-GatewaySourceStamp {
    $files = @()
    foreach ($p in @(
            (Join-Path $crateDir "Cargo.toml"),
            (Join-Path $root "Cargo.toml"),
            (Join-Path $root "Cargo.lock")
        )) {
        if (Test-Path -LiteralPath $p) {
            $files += Get-Item -LiteralPath $p
        }
    }
    $files += Get-ChildItem -Path $crateDir -Recurse -File -Include *.rs -ErrorAction SilentlyContinue
    if (-not $files) { return $null }
    return ($files | Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1)
}

# ── 0) 前置检查 ────────────────────────────────────────────────────────
if (-not (Test-Path -LiteralPath (Join-Path $crateDir "src/bin/gateway.rs"))) {
    throw "未找到网关源码：$crateDir/src/bin/gateway.rs"
}

# cargo 工具链：优先 PATH，其次常见安装位置。
#
# 为什么显式找：本机的 `cargo` 与 `go` 都不在全新 shell 的 PATH 里
#（见 AGENTS.md 记录的坑）。直接调 `cargo` 会报 "program not found"，
# 而那个报错**完全不提 PATH**，极易被误判成"环境坏了"。
function Find-Cargo {
    $cmd = Get-Command cargo -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    foreach ($cand in @(
            (Join-Path $env:USERPROFILE ".cargo\bin\cargo.exe"),
            "C:\Users\$env:USERNAME\.cargo\bin\cargo.exe",
            "$env:LOCALAPPDATA\Programs\cargo\bin\cargo.exe",
            "C:\cargo\bin\cargo.exe"
        )) {
        if ($cand -and (Test-Path -LiteralPath $cand)) { return $cand }
    }
    return $null
}

# ── 1) 判断是否需要重建 ────────────────────────────────────────────────
$needBuild = $true
$reason = ""
if ($Force) {
    $reason = "-Force"
} elseif (-not (Test-Path -LiteralPath $gwExe)) {
    $reason = "产物不存在"
} else {
    $exeTime = (Get-Item -LiteralPath $gwExe).LastWriteTimeUtc
    $src = Get-GatewaySourceStamp
    if ($null -eq $src) {
        throw "在 $crateDir 下没找到任何 .rs 文件 —— 源码目录不对？"
    }
    if ($src.LastWriteTimeUtc -gt $exeTime) {
        $reason = "源码比产物新（$($src.Name)）"
    } else {
        $needBuild = $false
    }
}

if ($CheckOnly) {
    if (-not (Test-Path -LiteralPath $gwExe)) {
        throw "网关产物不存在：$gwExe（请运行 scripts/build-gateway.ps1）"
    }
    $src = Get-GatewaySourceStamp
    if ($src -and $src.LastWriteTimeUtc -gt (Get-Item -LiteralPath $gwExe).LastWriteTimeUtc) {
        throw "网关产物**过期**（$($src.Name) 比它新）：请运行 scripts/build-gateway.ps1 重建"
    }
    Say "    网关产物是最新的：$gwExe" Green
    return
}

if (-not $needBuild) {
    $age = (Get-Date).ToUniversalTime() - (Get-Item -LiteralPath $gwExe).LastWriteTimeUtc
    Say ("    网关产物已是最新（{0:N1} 小时前构建），跳过重建" -f $age.TotalHours) Green
} else {
    Say "==> 构建 Rust 网关（$profile；$reason）" Cyan

    $cargoExe = Find-Cargo
    if (-not $cargoExe) {
        throw @"
未找到 cargo 可执行文件。

Rust 工具链不在 PATH 里，且常见安装位置也没有。
请安装 Rust（https://rustup.rs/）或把 cargo.exe 所在目录加入 PATH。
"@
    }

    New-Item -ItemType Directory -Force -Path $embeddedDir | Out-Null

    $profileFlag = @()
    if (-not $DebugBuild) { $profileFlag = @("--release") }
    $extra = @()
    if ($CargoArgs.Trim()) { $extra = $CargoArgs.Trim() -split '\s+' }

    Push-Location $root
    try {
        # ⚠ npm/cargo 会把**普通进度信息**写到 stderr。PowerShell 5.1 在
        # `$ErrorActionPreference = 'Stop'` 下会把原生命令的 stderr 当成
        # **致命错误**并中止脚本 —— 而 cargo 的 "Compiling …" 恰恰走 stderr。
        # 故这一段临时把 stderr 当普通输出流，并**只**用 $LASTEXITCODE 判成败。
        $prevEAP = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            & $cargoExe build -p ai-gateway-router --bin gateway @profileFlag @extra 2>&1 |
                ForEach-Object { Say "    $_" }
            $code = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $prevEAP
        }
    } finally {
        Pop-Location
    }

    if ($code -ne 0) {
        throw @"
网关构建失败（cargo build exit $code）。

若报错是「failed to download / no matching package」类，那是**网络问题**，
不是代码问题。本机的 crates.io 常不可达，两种解法：
    · 在 .cargo/config.toml 里把 crates-io 换到可达镜像；
    · 依赖已进本地缓存时，用 `-CargoArgs "--offline"` 重跑。
"@
    }

    # cargo 的产物目录名与 profile 同名（release / debug）。CARGO_TARGET_DIR
    # 可被外部改写，故先按它算，相对路径按仓库根归一。
    $targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "target" }
    if (-not [System.IO.Path]::IsPathRooted($targetDir)) {
        $targetDir = Join-Path $root $targetDir
    }
    $srcBin = Join-Path $targetDir "$profile/gateway.exe"
    if (-not (Test-Path -LiteralPath $srcBin)) {
        throw "未找到构建产物：$srcBin"
    }
    Copy-Item -LiteralPath $srcBin -Destination $gwExe -Force
    Say "    已从源码构建：$gwExe" Green
}

# ── 2) 记录指纹（供构建后校验"内嵌的确实是这一份"）────────────────────
$hash = (Get-FileHash -LiteralPath $gwExe -Algorithm SHA256).Hash
Set-Content -LiteralPath $gwSha -Value $hash -Encoding ascii -NoNewline
$size = [math]::Round((Get-Item -LiteralPath $gwExe).Length / 1MB, 2)

# 阈值与 build.rs 的「第一个 >1 MB 的候选胜出」一致：低于它 build.rs 会判为
# 「不是有效网关」而跳过内嵌，**静默**产出不内嵌网关的包。宁可在这里失败。
if ((Get-Item -LiteralPath $gwExe).Length -lt 1048576) {
    throw "网关产物异常（仅 $((Get-Item -LiteralPath $gwExe).Length) 字节，低于 build.rs 的 1 MB 阈值）"
}

Say ("    embedded/gateway.exe  {0} MB  SHA256 {1}" -f $size, $hash.Substring(0, 16)) Green

# 供调用方（build-signed.ps1）读取
Write-Output $hash
