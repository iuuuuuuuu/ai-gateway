<#
.SYNOPSIS
    统一 bump 版本号（PowerShell 版，等价于 scripts/bump-version.sh 并修掉其两个缺陷）。

.DESCRIPTION
    与 bump-version.sh 相比有两处修正：

    1. **Cargo.lock 的根包**。原脚本的 sed 锚点是
       `^name = "ai-gateway-(core|gateway|rust|server)"`，匹配不到根包
       `name = "ai-gateway"`（它后面没有连字符后缀），于是根包版本会留在旧值，
       使 `cargo build --locked` 失败。这里显式包含根包。
    2. **package-lock.json**。原脚本完全不碰它，导致 package.json 与
       package-lock.json 的 version 长期不一致（实测 1.0.0 vs 0.5.0）。

    仍然锚定包名替换 Cargo.lock：第三方 crate（如 defmt-parser 1.0.0）
    的版本号可能恰好相同，全局替换会误伤依赖锁。

.EXAMPLE
    pwsh scripts/bump-version.ps1 1.0.1
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string]$Version
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "版本号格式应为 x.y.z，收到：$Version"
}

$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

function Set-FileVersion {
    param([string]$Path, [string]$Pattern, [string]$Replacement, [string]$Label)
    if (-not (Test-Path -LiteralPath $Path)) { throw "文件不存在：$Path" }
    $text = [System.IO.File]::ReadAllText($Path)
    $new = [regex]::Replace($text, $Pattern, $Replacement)
    if ($new -eq $text) {
        Write-Host "  跳过（无匹配）：$Path"
        return 0
    }
    $n = ([regex]::Matches($text, $Pattern)).Count
    [System.IO.File]::WriteAllText($Path, $new, $utf8NoBom)
    Write-Host "  $Label  $Path（$n 处）"
    return $n
}

# 只替换紧随包名之后的那一行版本，避免误伤同名版本的第三方 crate。
function Set-LockedCrateVersion {
    param([string]$Path, [string[]]$CrateNames)
    $lines = [System.IO.File]::ReadAllLines($Path)
    $hit = 0
    for ($i = 0; $i -lt $lines.Count - 1; $i++) {
        if ($lines[$i] -match '^name = "(.+)"$' -and $CrateNames -contains $Matches[1]) {
            if ($lines[$i + 1] -match '^version = "\d+\.\d+\.\d+"$') {
                $lines[$i + 1] = "version = ""$Version"""
                $hit++
            }
        }
    }
    [System.IO.File]::WriteAllLines($Path, $lines, $utf8NoBom)
    Write-Host "  Cargo.lock  $Path（$hit 个 workspace 包）"
    return $hit
}

Write-Host "==> 版本 bump 到 $Version"

# ── 1. JSON 清单 ──────────────────────────────────────────────────────────
$jsonPattern = '"version": "\d+\.\d+\.\d+"'
foreach ($f in @('package.json', 'src-tauri/tauri.conf.json', 'npm/package.json')) {
    [void](Set-FileVersion -Path $f -Pattern $jsonPattern -Replacement "`"version`": `"$Version`"" -Label 'JSON ')
}

# ── 2. 各 Cargo.toml 顶层 version ─────────────────────────────────────────
$tomlPattern = '(?m)^version = "\d+\.\d+\.\d+"'
foreach ($f in @(
        'src-tauri/Cargo.toml',
        'crates/ai-gateway-core/Cargo.toml',
        'crates/ai-gateway-server/Cargo.toml',
        'crates/ai-gateway-router/Cargo.toml')) {
    [void](Set-FileVersion -Path $f -Pattern $tomlPattern -Replacement "version = `"$Version`"" -Label 'TOML ')
}

# ── 3. 平台包 ─────────────────────────────────────────────────────────────
foreach ($f in (Get-ChildItem 'npm/platform/*/package.json')) {
    [void](Set-FileVersion -Path $f.FullName -Pattern $jsonPattern -Replacement "`"version`": `"$Version`"" -Label '平台 ')
}

# ── 4. npm/package.json 的 optionalDependencies ──────────────────────────
$npmPkgPath = 'npm/package.json'
$npmPkg = [System.IO.File]::ReadAllText($npmPkgPath) | ConvertFrom-Json
foreach ($k in @($npmPkg.optionalDependencies.PSObject.Properties.Name)) {
    $npmPkg.optionalDependencies.$k = $Version
}
# 保持仓库既有的 2 空格缩进 + 末尾换行，并统一为 LF。
$json = ($npmPkg | ConvertTo-Json -Depth 20) -replace "`r`n", "`n"
[System.IO.File]::WriteAllText($npmPkgPath, $json + "`n", $utf8NoBom)
Write-Host "  optionalDependencies  $npmPkgPath（$($npmPkg.optionalDependencies.PSObject.Properties.Name.Count) 项）"

# ── 5. Cargo.lock（锚定包名，含根包） ─────────────────────────────────────
[void](Set-LockedCrateVersion -Path 'Cargo.lock' -CrateNames @(
        'ai-gateway', 'ai-gateway-core', 'ai-gateway-router', 'ai-gateway-server'))

# ── 6. package-lock.json 根 version ───────────────────────────────────────
# **必须锚定 "name": "ai-gateway"**：package-lock.json 里有 270 处
# `"version": "x.y.z"`（每个依赖一个），用上面那条全局 pattern 会把所有依赖
# 的版本号一起改成目标版本，直接毁掉 lockfile。
$lockPattern = '("name": "ai-gateway",\s*"version": ")\d+\.\d+\.\d+'
[void](Set-FileVersion -Path 'package-lock.json' -Pattern $lockPattern -Replacement "`${1}$Version" -Label 'LOCK ')

Write-Host "==> 完成，所有版本已同步为 $Version"
