# build-single.ps1 —— 构建单文件 AI Gateway（内含 OpenAI 兼容网关）
#
# 产物：dist-single/ai-gateway.exe （一个文件，无需额外的 gateway.exe）
#
# 流程：
#   1. 构建网关（**Rust**），输出到 crates/ai-gateway-core/embedded/gateway.exe
#   2. 构建前端（rust-embed 需要仓库根 dist/）
#   3. cargo build：build.rs 会把网关 gzip 后编进主程序
#
# 依赖：Node >= 16、Rust（MinGW 亦可，无需 Visual Studio）。
#       ⚠ **不再需要 Go**（2026-10-01 起网关是 Rust 的，见 build-gateway.ps1）。
[CmdletBinding()]
param(
    [string]$GatewaySource = "",
    [string]$OutputDir = "dist-single",
    [switch]$SkipFrontend,
    # 跳过网关重建，直接沿用 embedded/ 下已有的产物。
    # 网关内容没变时，重建会刷新文件 mtime，进而让 ai-gateway-core 重编一次 ——
    # 只想改 Rust/前端时用这个开关省掉这一轮。
    [switch]$SkipGateway,
    # 额外传给 cargo 的参数，如 "--offline"。受限网络下必用。
    [string]$CargoArgs = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot   # 仓库根
Push-Location $root
try {
    $embedded = Join-Path $root "crates/ai-gateway-core/embedded"
    New-Item -ItemType Directory -Force -Path $embedded | Out-Null

    # ---- 1) 构建网关 ----
    Write-Host "==> [1/4] 构建网关 (Rust)" -ForegroundColor Cyan
    if ($GatewaySource) {
        Copy-Item $GatewaySource (Join-Path $embedded "gateway.exe") -Force
        Write-Host "    直接使用: $GatewaySource"
    } elseif ($SkipGateway -and (Test-Path (Join-Path $embedded "gateway.exe"))) {
        Write-Host "    跳过（-SkipGateway，沿用现有 embedded/gateway.exe）"
    } else {
        # ⚠ 这里**不再**自己判断"源码比产物新"。
        #
        # 原逻辑（Go 版）在本文件里复制了一份「.go 文件 mtime vs 产物 mtime」的
        # 判断，与 scripts/build-gateway.ps1 里那份**是同一个逻辑的两份拷贝**。
        # 两份拷贝的必然结局是漂移：改了其中一份的判据（比如把 go.mod 也纳入
        # 指纹），另一份不会跟着变，于是"用哪个脚本构建"会得出不同的新鲜度结论。
        #
        # 现在统一委托给 build-gateway.ps1 —— 它已是**唯一**的网关构建入口，
        # 判据、cargo 定位、1 MB 阈值校验、指纹落盘都在那一处。
        #
        # 为什么保留 `-SkipGateway` 而不是删掉：它是**性能**开关（跳过重建可省
        # 一次 ai-gateway-core 重编），语义明确，且调用点都在人手里。
        #
        # ⚠ 这里**不检查 `$LASTEXITCODE`**：它只对**原生命令**有意义，调用
        # `.ps1` 之后它保留的是更早某个原生命令的残留值（可能是 0，也可能是
        # 上次失败留下的非 0）—— 拿它判断会得出随机结论。被调脚本内部用的是
        # `throw`，在本脚本 `$ErrorActionPreference = "Stop"` 下会直接向上抛出，
        # 本来就到不了下一行。这里只把它的哈希输出收进变量，避免混进日志。
        $gwHash = & (Join-Path $PSScriptRoot "build-gateway.ps1") -CargoArgs $CargoArgs
        Write-Host ("    网关指纹 {0}…" -f $gwHash.Substring(0, 16))
    }

    # ---- 2) 前端 ----
    Write-Host "==> [2/4] 构建前端" -ForegroundColor Cyan
    if (-not $SkipFrontend) {
        Push-Location $root   # 前端源码与输出目录（dist/）都在仓库根
        if (-not (Test-Path node_modules)) { npm install --no-audit --no-fund }
        npm run build
        $code = $LASTEXITCODE
        Pop-Location
        if ($code -ne 0) { throw "前端构建失败" }
    } else {
        Write-Host "    跳过（沿用现有 dist/）"
    }

    # ---- 3) Rust 编译（内嵌网关 + 前端）----
    Write-Host "==> [3/4] 编译 Rust 主体" -ForegroundColor Cyan
    # ⚠ cargo 会把**普通进度信息**写到 stderr（"Compiling …"、"Finished …"）。
    # PowerShell 5.1 在 `$ErrorActionPreference = 'Stop'` 下会把原生命令的 stderr
    # 当成**致命错误**并当场中止脚本 —— 于是永远走不到下一行的 $LASTEXITCODE
    # 判断，表现是「跑到 [3/4] 就没声了，也没有任何报错」（2026-10-06 实测：
    # dist-single/ 根本没生成，而日志里连一句失败都没有）。
    # 故这一段临时把 stderr 当普通输出流，并**只**用 $LASTEXITCODE 判成败。
    # 与 scripts/build-gateway.ps1:191-202 是同一处理，改一处请同步另一处。
    $prevEap = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        cargo build -p ai-gateway-server --release 2>&1 |
            ForEach-Object { Write-Host "    $_" }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prevEap
    }
    if ($code -ne 0) { throw "Rust 构建失败（cargo build exit $code）" }

    # ---- 4) 输出 ----
    Write-Host "==> [4/4] 拷贝产物" -ForegroundColor Cyan
    # 输出目录必须与 dist/（前端资源，会被 rust-embed 内嵌）分开。
    # 若把 exe 放进 dist/，下一轮构建会把上一轮的 exe 也嵌进去，体积翻倍。
    $outPath = Join-Path $root $OutputDir
    if ($outPath -eq (Join-Path $root "dist")) {
        throw "输出目录不能是 dist/（该目录是前端资源，会被内嵌进二进制，导致体积翻倍）"
    }
    New-Item -ItemType Directory -Force -Path $outPath | Out-Null
    $exe = Join-Path $root "target/release/ai-gateway.exe"
    Copy-Item $exe (Join-Path $root "$OutputDir/ai-gateway.exe") -Force

    $size = (Get-Item (Join-Path $root "$OutputDir/ai-gateway.exe")).Length / 1MB
    if ($size -gt 30) {
        Write-Warning ("产物 {0:N1} MB 偏大，可能把旧的 exe 也内嵌了；请检查 dist/ 下是否混入了 exe" -f $size)
    }
    Write-Host ""
    Write-Host "==> 完成：单文件分发（无需额外的 gateway.exe）" -ForegroundColor Green
    Write-Host ("    {0}  {1:N2} MB" -f "$OutputDir/ai-gateway.exe", $size)
    Write-Host ""
    Write-Host "运行: $OutputDir/ai-gateway.exe"
    Write-Host "  账号管理: http://127.0.0.1:57890/"
    Write-Host "  兼容网关: 在「兼容网关」页面启动"
}
finally {
    Pop-Location
}
