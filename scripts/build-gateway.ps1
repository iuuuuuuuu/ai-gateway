# build-gateway.ps1 —— 构建 Go 网关并放入 embedded/，供 Tauri 内嵌。
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
# 于是：谁改了 go-gateway/ 的代码、但没手动重建 embedded/gateway.exe，
# **打出来的安装包就内嵌旧网关** —— 症状是"新功能在开发机上好好的，
# 装完却没有"，而构建日志里没有任何异常。
#
# 修法不是"记得先手动跑一次构建"（那正是会忘的事），而是：
#
#	1. 本脚本把「构建网关」变成**一条命令**；
#	2. build-signed.ps1 在构建前**自动调用**它（见那里的 [0/4] 步）；
#	3. 加**指纹校验**：构建前后比对 embedded 网关的 SHA256，
#	   对不上就**直接失败**，让"内嵌旧网关"在物理上无法交付。
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
    [switch]$Quiet
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot          # 仓库根
$gwDir = Join-Path $root "go-gateway"
$embeddedDir = Join-Path $root "crates/ai-gateway-core/embedded"
$gwExe = Join-Path $embeddedDir "gateway.exe"
$gwSha = Join-Path $embeddedDir "gateway.exe.sha256"

function Say([string]$msg, [string]$color = "Gray") {
    if (-not $Quiet) { Write-Host $msg -ForegroundColor $color }
}

# ── 源码指纹：go-gateway 下所有 .go / go.mod / go.sum 的最新 mtime ──────
#
# 为什么用 mtime 而不是内容哈希：
#   · 构建脚本要快，哈希几万个文件不值得；
#   · 哈希也挡不住"改了文件但 mtime 没变"这种需要手动 touch 的极罕见情况。
# mtime 的假阳性（touch 过但内容没变）代价只是一次多余的重建。
function Get-GatewaySourceStamp {
    $files = Get-ChildItem -Path $gwDir -Recurse -File -Include *.go, go.mod, go.sum -ErrorAction SilentlyContinue
    if (-not $files) { return $null }
    return ($files | Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1)
}

# ── 1) 判断是否需要重建 ────────────────────────────────────────────────
if (-not (Test-Path -LiteralPath $gwDir)) {
    throw "未找到网关源码目录：$gwDir"
}

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
        throw "在 $gwDir 下没找到任何 .go 文件 —— 源码目录不对？"
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
    Say "==> 构建 Go 网关（$reason）" Cyan

    # Go 工具链：优先 PATH，其次常见安装位置。
    #
    # 为什么显式找：本机的 `cargo` 与 `go` 都不在全新 shell 的 PATH 里
    #（见 AGENTS.md 记录的坑）。直接调 `go` 会报 "program not found"，
    # 而那个报错**完全不提 PATH**，极易被误判成"环境坏了"。
    $goExe = $null
    $cmd = Get-Command go -ErrorAction SilentlyContinue
    if ($cmd) {
        $goExe = $cmd.Source
    } else {
        foreach ($cand in @(
                "C:\Program Files\Go\bin\go.exe",
                "$env:LOCALAPPDATA\Programs\Go\bin\go.exe",
                "$env:USERPROFILE\go\bin\go.exe",
                "D:\Go\bin\go.exe"
            )) {
            if (Test-Path -LiteralPath $cand) { $goExe = $cand; break }
        }
    }
    if (-not $goExe) {
        throw @"
未找到 go 可执行文件。

Go 工具链不在 PATH 里，且常见安装位置也没有。
请安装 Go（https://go.dev/dl/）或把 go.exe 所在目录加入 PATH。
"@
    }

    New-Item -ItemType Directory -Force -Path $embeddedDir | Out-Null

    # ⚠ 构建缓存必须可写。
    #
    # 受限沙箱下 `%LOCALAPPDATA%\go-build` 可能不可写，报
    # `open ...go-build\...: Access is denied` —— 那个报错不提示"是权限"，
    # 很容易被当成代码问题。这里允许用环境变量覆盖，并在失败时给出提示。
    $prevGoFlags = $env:GOFLAGS
    $prevGoCache = $env:GOCACHE
    $prevGoTmp = $env:GOTMPDIR
    try {
        if (-not $env:GOFLAGS) { $env:GOFLAGS = "-mod=mod" }
        if (-not $env:GOCACHE) {
            # 默认缓存不可写时回退到仓库内的 test-bin（与既有验证约定一致）
            $fallback = Join-Path $root "..\test-bin\.gocache"
            if (Test-Path -LiteralPath (Split-Path $fallback -Parent)) {
                $env:GOCACHE = [System.IO.Path]::GetFullPath($fallback)
                $env:GOTMPDIR = [System.IO.Path]::GetFullPath((Join-Path $root "..\test-bin\.gotmp"))
                New-Item -ItemType Directory -Force -Path $env:GOCACHE, $env:GOTMPDIR | Out-Null
            }
        }

        Push-Location $gwDir
        try {
            # -trimpath：去掉绝对路径，产物可复现（也避免泄露本机目录结构）
            # -s -w    ：剥离符号表，体积从约 14 MB 降到约 10 MB
            & $goExe build -trimpath -ldflags "-s -w" -o $gwExe ./cmd/server
            $code = $LASTEXITCODE
        } finally {
            Pop-Location
        }
        if ($code -ne 0) {
            throw @"
网关构建失败（go build exit $code）。

若报错是 `Access is denied` 且路径含 `go-build`，那是**构建缓存不可写**，
不是代码问题：设 GOCACHE 到一个可写目录再重试，例如
    `$env:GOCACHE = "D:\WishProject\WorkbuddySwitchAPi\test-bin\.gocache"
"@
        }
    } finally {
        $env:GOFLAGS = $prevGoFlags
        $env:GOCACHE = $prevGoCache
        $env:GOTMPDIR = $prevGoTmp
    }
    Say "    已从源码构建：$gwExe" Green
}

# ── 2) 记录指纹（供构建后校验"内嵌的确实是这一份"）────────────────────
$hash = (Get-FileHash -LiteralPath $gwExe -Algorithm SHA256).Hash
Set-Content -LiteralPath $gwSha -Value $hash -Encoding ascii -NoNewline
$size = [math]::Round((Get-Item -LiteralPath $gwExe).Length / 1MB, 2)
Say ("    embedded/gateway.exe  {0} MB  SHA256 {1}" -f $size, $hash.Substring(0, 16)) Green

# 供调用方（build-signed.ps1）读取
Write-Output $hash
