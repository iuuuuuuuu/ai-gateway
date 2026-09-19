# build-single.ps1 —— 构建单文件 AI Gateway（内含 OpenAI 兼容网关）
#
# 产物：dist/ai-gateway.exe （一个文件，无需额外的 gateway.exe）
#
# 流程：
#   1. 用 Go 构建网关，输出到 crates/ai-gateway-core/embedded/gateway.exe
#   2. 构建前端（rust-embed 需要仓库根 dist/）
#   3. cargo build：build.rs 会把网关 gzip 后编进主程序
#
# 依赖：Go >= 1.22、Node >= 16、Rust（MinGW 亦可，无需 Visual Studio）
[CmdletBinding()]
param(
    [string]$GatewaySource = "",
    [string]$OutputDir = "dist-single",
    [switch]$SkipFrontend,
    # 跳过 Go 网关重建，直接沿用 embedded/ 下已有的产物。
    # 网关内容没变时，重建会刷新文件 mtime，进而让 ai-gateway-core 重编一次（约 25s）——
    # 只想改 Rust/前端时用这个开关省掉这一轮。
    [switch]$SkipGateway
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot   # 仓库根
Push-Location $root
try {
    $embedded = Join-Path $root "crates/ai-gateway-core/embedded"
    New-Item -ItemType Directory -Force -Path $embedded | Out-Null

    # ---- 1) 构建网关 ----
    Write-Host "==> [1/4] 构建网关 (Go)" -ForegroundColor Cyan
    if ($GatewaySource) {
        Copy-Item $GatewaySource (Join-Path $embedded "gateway.exe") -Force
        Write-Host "    直接使用: $GatewaySource"
    } elseif ($SkipGateway -and (Test-Path (Join-Path $embedded "gateway.exe"))) {
        Write-Host "    跳过（-SkipGateway，沿用现有 embedded/gateway.exe）"
    } else {
        # ⚠ 这里**不再**「已有就沿用」。
        #
        # 原逻辑是：embedded/gateway.exe 存在就打印一行提示然后**跳过重建**。
        # 那个默认行为已经害过我们两次 —— 打完包才发现里面装的是旧网关
        # （症状是"新功能在开发机上好好的，装完却没有"，
        # 而日志里只有一行容易被忽略的"已有 ... 如需重建请加 -GatewaySource"）。
        #
        # 现在改成：**源码比产物新就自动重建**；只有显式 `-SkipGateway`
        # 才跳过。判断依据是 go-gateway 下所有 .go 文件与 go.mod 的
        # 最新修改时间 vs 产物时间。
        #
        # 为什么用 mtime 而不是内容哈希：构建脚本要快，且哈希也挡不住
        # "改了文件但 mtime 没变"这种极罕见情况（那需要手动 touch）。
        # mtime 的假阳性（touch 过但内容没变）代价只是一次多余的重建。
        $gwExe = Join-Path $embedded "gateway.exe"
        $gwDir = Join-Path $root "go-gateway"
        $needRebuild = $true
        if (Test-Path $gwExe) {
            $exeTime = (Get-Item $gwExe).LastWriteTimeUtc
            $srcTime = Get-ChildItem -Path $gwDir -Recurse -File -Include *.go, go.mod, go.sum -ErrorAction SilentlyContinue |
                Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
            if ($srcTime -and $srcTime.LastWriteTimeUtc -le $exeTime) {
                $needRebuild = $false
                $age = (Get-Date).ToUniversalTime() - $exeTime
                Write-Host ("    网关产物比源码新（{0:N0} 小时前构建），无需重建" -f $age.TotalHours)
            } else {
                $newer = if ($srcTime) { $srcTime.Name } else { "（未知）" }
                Write-Host "    源码比产物新（$newer），自动重建网关" -ForegroundColor Yellow
            }
        }
        if ($needRebuild) {
            if (-not (Test-Path $gwDir)) {
                throw "未找到网关源码目录 $gwDir；可用 -GatewaySource 指定已有的 gateway.exe"
            }
            $env:GOFLAGS = "-mod=mod"
            Push-Location $gwDir
            go build -trimpath -ldflags "-s -w" -o $gwExe ./cmd/server
            $code = $LASTEXITCODE
            Pop-Location
            if ($code -ne 0) { throw "网关构建失败" }
            Write-Host "    已从源码构建网关"
        }
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
    cargo build -p ai-gateway-server --release
    if ($LASTEXITCODE -ne 0) { throw "Rust 构建失败" }

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
