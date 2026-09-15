<#
.SYNOPSIS
    把本仓库从 "AI Gateway / ai-gateway" 彻底更名为 "AI Gateway"。

.DESCRIPTION
    分两阶段执行：
      ① 文本内容替换（按「最长优先」顺序，避免前缀互相吞噬）；
      ② 路径/文件重命名（crate 目录、npm 包目录、npm bin 入口）。

    刻意保留的标识（改名会破坏兼容或语义）：
      - `WorkBuddy` / `CodeBuddy` / `copilot.tencent.com` 等**上游产品名**；
      - `~/.codebuddy`、`workbuddy-desktop.info` 等**官方客户端**的路径与文件名；
      - 网关凭证目录、账号库 JSON 的**内部字段名**（如 `workbuddy_desktop`）；
      - `is_workbuddy_image_name` 等函数名（描述的是官方客户端，不是本软件）。

    注意：`.ai-gateway` → `.ai-gateway` 由规则 `ai-gateway` → `ai-gateway` 自然覆盖
    （`.ai-gateway` 含子串 `ai-gateway`），无需单独规则。

.PARAMETER DryRun
    只打印将要发生的替换统计，不写盘。
#>
[CmdletBinding()]
param(
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# ── 扫描范围：源码与文档；排除构建产物与工具记忆 ────────────────────────────
$includeExt = @(
    '*.rs', '*.ts', '*.tsx', '*.json', '*.toml', '*.ps1', '*.nsi', '*.mjs', '*.js',
    '*.yml', '*.yaml', '*.md', '*.html', '*.sh', '*.py', '*.cjs', '*.cmd', '*.css'
)
$excludeDir = '\\(target|node_modules|dist|dist-single|_rel|__pycache__|\.git|\.workbuddy)\\'

$files = Get-ChildItem -Path $root -Recurse -File -Include $includeExt |
    Where-Object { $_.FullName -notmatch $excludeDir }

# ── 替换规则（顺序敏感：长串必须排在短串之前）──────────────────────────────
# 用「扁平数组 + 步长 2」而不是嵌套数组/哈希表：
#   - 哈希表：PowerShell 的键默认大小写不敏感，`ai-gateway` 与 `AI-GATEWAY` 会判定为
#     重复键直接报错；
#   - 嵌套数组：`@(@('a','b'), @('c','d'))` 会被 `@()` 展平成一维，`$pair[0]` 退化成
#     单个字符——本脚本第一版就踩了这个坑，表现为「把 W 替换成 B」这种灾难性结果。
# 字符串的 .Replace() 是大小写敏感的，正是这里需要的语义。
$rulePairs = @(
    # 环境变量
    'AI_GATEWAY_ROUTER_BIN',         'AI_GATEWAY_ROUTER_BIN'
    'AI_GATEWAY_SCREENSHOT_DEMO',     'AI_GATEWAY_SCREENSHOT_DEMO'
    'AI_GATEWAY_ACCOUNTS_FILE',       'AI_GATEWAY_ACCOUNTS_FILE'
    'AI_GATEWAY_BINARY',              'AI_GATEWAY_BINARY'
    'AI_GATEWAY_HOME',                'AI_GATEWAY_HOME'
    'AI_GATEWAY_PROBE_TIMEOUT_SEC',          'AI_GATEWAY_PROBE_TIMEOUT_SEC'
    # Rust 标识符（下划线形）
    'ai_gateway_lib',            'ai_gateway_lib'
    'ai_gateway_core',                'ai_gateway_core'
    'ai_gateway_router',             'ai_gateway_router'
    'ai_gateway_test_',               'ai_gateway_test_'
    'ai_gateway',                     'ai_gateway'
    # crate / 包名（连字符形）——长名优先
    'ai-gateway',      'ai-gateway'
    'ai-gateway-core',                'ai-gateway-core'
    'ai-gateway-router',             'ai-gateway-router'
    'ai-gateway-server',              'ai-gateway-server'
    'ai-gateway',                'ai-gateway'
    'ai-gateway',              'ai-gateway'
    'ai-gateway',                     'ai-gateway'
    # 大写/显示名形态
    'AI-GATEWAY',                     'AI-GATEWAY'
    'AI Gateway',      'AI Gateway'
    'AI_Gateway',      'AI_Gateway'
    'AI.Gateway',      'AI.Gateway'
    'com.momo0410.aigateway', 'com.momo0410.aigateway'
)

if ($rulePairs.Count % 2 -ne 0) { throw "规则数组必须成对出现（当前 $($rulePairs.Count) 项）" }

$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$touched = 0
$totalHits = 0
$perRule = @{}
for ($i = 0; $i -lt $rulePairs.Count; $i += 2) { $perRule[$rulePairs[$i]] = 0 }

foreach ($file in $files) {
    $bytes = [System.IO.File]::ReadAllBytes($file.FullName)
    # 跳过二进制（含 NUL 字节）
    if ($bytes -contains 0) { continue }

    $hadBom = $bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF
    $text = [System.Text.Encoding]::UTF8.GetString($bytes)
    $original = $text

    for ($i = 0; $i -lt $rulePairs.Count; $i += 2) {
        $key = $rulePairs[$i]
        $value = $rulePairs[$i + 1]
        $count = ([regex]::Matches($text, [regex]::Escape($key))).Count
        if ($count -gt 0) {
            $perRule[$key] += $count
            $totalHits += $count
            $text = $text.Replace($key, $value)
        }
    }

    if ($text -ne $original) {
        $touched++
        if (-not $DryRun) {
            # 保留原 BOM 状态（build-single.ps1 原本带 BOM，PowerShell 5.1 需要它）
            if ($hadBom) {
                [System.IO.File]::WriteAllText($file.FullName, $text, (New-Object System.Text.UTF8Encoding($true)))
            } else {
                [System.IO.File]::WriteAllText($file.FullName, $text, $utf8NoBom)
            }
        }
    }
}

Write-Host "== 内容替换 ==" -ForegroundColor Cyan
for ($i = 0; $i -lt $rulePairs.Count; $i += 2) {
    $key = $rulePairs[$i]
    if ($perRule[$key] -gt 0) {
        Write-Host ("  {0,-32} -> {1,-28} {2,5} 处" -f $key, $rulePairs[$i + 1], $perRule[$key])
    }
}
Write-Host ("  文件 {0} 个，命中 {1} 处" -f $touched, $totalHits) -ForegroundColor Green

# ── 路径重命名 ──────────────────────────────────────────────────────────────
$pathRenames = @(
    @{ From = 'crates\ai-gateway-core';                  To = 'crates\ai-gateway-core' }
    @{ From = 'crates\ai-gateway-router';               To = 'crates\ai-gateway-router' }
    @{ From = 'crates\ai-gateway-server';                To = 'crates\ai-gateway-server' }
    @{ From = 'npm\bin\ai-gateway.js';            To = 'npm\bin\ai-gateway.js' }
    @{ From = 'npm\platform\ai-gateway-darwin-arm64'; To = 'npm\platform\ai-gateway-darwin-arm64' }
    @{ From = 'npm\platform\ai-gateway-darwin-x64';   To = 'npm\platform\ai-gateway-darwin-x64' }
    @{ From = 'npm\platform\ai-gateway-linux-arm64';  To = 'npm\platform\ai-gateway-linux-arm64' }
    @{ From = 'npm\platform\ai-gateway-linux-x64';    To = 'npm\platform\ai-gateway-linux-x64' }
    @{ From = 'npm\platform\ai-gateway-win32-x64';    To = 'npm\platform\ai-gateway-win32-x64' }
)

Write-Host "`n== 路径重命名 ==" -ForegroundColor Cyan
foreach ($rename in $pathRenames) {
    $from = Join-Path $root $rename.From
    $to = Join-Path $root $rename.To
    if (-not (Test-Path -LiteralPath $from)) {
        Write-Host ("  跳过（不存在）: {0}" -f $rename.From) -ForegroundColor DarkGray
        continue
    }
    if ($DryRun) {
        Write-Host ("  [WhatIf] {0} -> {1}" -f $rename.From, $rename.To)
        continue
    }
    Move-Item -LiteralPath $from -Destination $to
    Write-Host ("  {0} -> {1}" -f $rename.From, $rename.To) -ForegroundColor Green
}

if ($DryRun) {
    Write-Host "`n[WhatIf] 未写入任何更改。" -ForegroundColor Yellow
} else {
    Write-Host "`n完成。请检查 cargo build 与 npm run build。" -ForegroundColor Green
}

