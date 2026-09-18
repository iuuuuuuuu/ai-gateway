# build-gateway.ps1 —— 构建网关（Rust），产物落到 embedded/ 供内嵌。
#
# 产物：crates/ai-gateway-core/embedded/gateway.exe
# 之后 cargo build 时 build.rs 会把它 gzip 压缩后编进主程序，实现单文件分发。
#
# 与 scripts/build-gateway.sh（CI / Unix 用的等价脚本）产物路径完全相同，
# 因此宿主侧（gateway.rs 的 resolve_gateway_exe、build.rs 的 candidate_paths）
# 不需要任何改动。
#
# 用法：
#   pwsh scripts/build-gateway.ps1              # release 构建
#   pwsh scripts/build-gateway.ps1 -DevBuild    # debug 构建（快，便于调试）
#   pwsh scripts/build-gateway.ps1 -TargetDir D:\tmp\target   # 复用已有 target 目录
#
# 注意：cargo 可能不在 PATH 里（见 AGENTS.md「构建与签名」），本脚本会自动补上
# %USERPROFILE%\.cargo\bin。
[CmdletBinding()]
param(
    # 用 -DevBuild 而不是 -Debug：后者与 PowerShell 的公共参数 -Debug 冲突
    #（报 "parameter with the name 'Debug' was defined multiple times"）。
    [switch]$DevBuild,
    [string]$TargetDir = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot   # 仓库根

# cargo 兜底：本机 cargo 在 %USERPROFILE%\.cargo\bin，默认不在 PATH。
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
    if (Test-Path -LiteralPath $cargoBin) {
        $env:PATH = "$cargoBin;$env:PATH"
    }
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "未找到 cargo。请先安装 Rust 工具链，或把 %USERPROFILE%\.cargo\bin 加进 PATH。"
}

$embedded = Join-Path $root "crates/ai-gateway-core/embedded"
New-Item -ItemType Directory -Force -Path $embedded | Out-Null
$out = Join-Path $embedded "gateway.exe"

$profile = if ($DevBuild) { "dev" } else { "release" }
# 产物**目录名**与 profile 名不同：dev profile 的产物在 target/debug/ 下
#（cargo 的历史命名，`--release` 才用 release/）。用 profile 名当目录名会找不到产物。
$outDirName = if ($DevBuild) { "debug" } else { "release" }
Write-Host "==> [1/2] 构建 Rust 网关（$profile）" -ForegroundColor Cyan

$cargoArgs = @("build", "-p", "ai-gateway-router", "--bin", "gateway")
if (-not $DevBuild) { $cargoArgs += "--release" }
if ($TargetDir) { $cargoArgs += @("--target-dir", $TargetDir) }

Push-Location $root
try {
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build 失败（退出码 $LASTEXITCODE）" }
}
finally {
    Pop-Location
}

# 定位产物：指定了 --target-dir 时不在仓库的 target/ 下。
$targetRoot = if ($TargetDir) { $TargetDir } else { Join-Path $root "target" }
$built = Join-Path $targetRoot "$outDirName/gateway.exe"
if (-not (Test-Path -LiteralPath $built)) {
    throw "未找到构建产物 $built"
}

Write-Host "==> [2/2] 拷贝到 embedded/" -ForegroundColor Cyan
Copy-Item -LiteralPath $built -Destination $out -Force

$size = (Get-Item -LiteralPath $out).Length
if ($size -lt 1MB) {
    throw "产物异常（仅 $size 字节），疑似构建失败"
}
Write-Host ("    完成: $out ({0:N1} MB)" -f ($size / 1MB)) -ForegroundColor Green
Write-Host "    下一步: cargo build（build.rs 会自动压缩内嵌）"
