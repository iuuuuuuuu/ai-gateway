# 截图 / 演示模式启动（Tauri 桌面端）。
#
# 为什么需要这个脚本：原 npm script 是
#   AI_GATEWAY_SCREENSHOT_DEMO=1 VITE_DEMO_MODE=1 tauri dev
# 这是 POSIX 内联变量写法，在 Windows 的 PowerShell / cmd 下会直接失败
# （`'AI_GATEWAY_SCREENSHOT_DEMO' 不是内部或外部命令`）。仓库运行环境是 Windows
# （见 AGENTS.md Shell Policy），因此改为显式 PowerShell 设置环境变量。
#
# 两个变量各有用途，缺一不可：
#   - AI_GATEWAY_SCREENSHOT_DEMO=1  → Rust 侧 is_screenshot_demo()，跳过真实后台任务
#   - VITE_DEMO_MODE=1             → 前端 demoModeEnabled，只读数据由本地假数据提供
#
# 用法：pwsh scripts/dev-screenshot.ps1

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location -LiteralPath $repoRoot

$env:AI_GATEWAY_SCREENSHOT_DEMO = "1"
$env:VITE_DEMO_MODE = "1"

Write-Host "启动截图/演示模式（AI_GATEWAY_SCREENSHOT_DEMO=1, VITE_DEMO_MODE=1）…" -ForegroundColor Cyan
Write-Host "注意：此模式不读写真实账号数据，所有只读数据来自本地假数据。" -ForegroundColor DarkGray

if (Get-Command pnpm -ErrorAction SilentlyContinue) {
    pnpm exec tauri dev
} else {
    npx tauri dev
}
exit $LASTEXITCODE
