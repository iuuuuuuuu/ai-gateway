<#
.SYNOPSIS
    把本仓库从 "WorkBuddy Switch Gateway / wb-switch" 彻底更名为 "AI Gateway"。

.NOTES
    **本脚本已于 2026-09-15 执行完毕，仓库当前已是更名后的状态。**
    保留它是为了留存完整的替换规则清单（便于审计「哪些标识被改了、哪些刻意没改」），
    重复执行是幂等的（找不到旧名就什么都不做），但正常情况下不需要再跑。

    已知的遗漏与补救（脚本未覆盖，已单独修复）：
      - 侧边栏硬编码标题 "WorkBuddy Switch"（改为 AI Gateway）
      - 托盘 tooltip "ai-gateway"（改为显示名 AI Gateway）
      - agent_import 的 PROVIDER_NAME（改为 AI Gateway）
      - 构建脚本/发布工作流里的产品显示名残留
      - 上游归属链接被误替换（已还原，许可证合规要求保留原作者与原始仓库）
#>
[CmdletBinding()]
param(
    [switch]$DryRun,
    # 明知风险仍要强制执行（默认拒绝，见下方守卫）
    [switch]$Force
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# ── 二次执行守卫（重要）────────────────────────────────────────────────────
#
# 更名**已完成**，本脚本现在是历史记录，不是可重复运行的工具。
#
# 再次实际运行会破坏三处**故意保留**的旧名引用：
#   - `migrate_store.rs` 的 `.wb-switch`：迁移来源目录，改了就读不到旧数据
#   - `build-signed.ps1` 的 `wb-switch-updater.*`：已发布密钥的实际文件名，改了签不了名
#   - README 的上游归属链接（changexbc/workbuddy-switch）：许可证合规要求保留
#
# 这些引用**看起来**正是脚本要清理的「残留」，因此误跑一次就会静默破坏迁移与签名。
# 默认拒绝执行，必须显式 `-Force` 才继续。
if (-not $DryRun -and -not $Force) {
    throw @"
拒绝执行：更名已完成，本脚本仅作历史记录保留。

再次实际运行会破坏三处故意保留的旧名引用：
  - crates/ai-gateway-core/src/modules/migrate_store.rs 的 `.wb-switch`（迁移来源目录）
  - scripts/build-signed.ps1 的 `wb-switch-updater.*`（已发布密钥的实际文件名）
  - README 的上游归属链接（许可证合规要求保留）

如需查看「当年改了哪些标识」，用 -DryRun（只统计不写盘）。
确实要强制执行请显式加 -Force。
"@
}

# ── 扫描范围：源码与文档；排除构建产物与工具记忆 ────────────────────────────
$includeExt = @(
    '*.rs', '*.ts', '*.tsx', '*.json', '*.toml', '*.ps1', '*.nsi', '*.mjs', '*.js',
    '*.yml', '*.yaml', '*.md', '*.html', '*.sh', '*.py', '*.cjs', '*.cmd', '*.css'
)
$excludeDir = '\\(target|node_modules|dist|dist-single|_rel|__pycache__|\.git|\.workbuddy)\\'

$files = Get-ChildItem -Path $root -Recurse -File -Include $includeExt |
    Where-Object { $_.FullName -notmatch $excludeDir } |
    # **必须排除脚本自身**：否则它会用自己的规则表改写自己 ——
    # 第一版实测把规则表的左列全部替换成了右列，规则退化成
    # 「AI_GATEWAY_HOME -> AI_GATEWAY_HOME」这种空操作，历史记录被抹掉。
    Where-Object { $_.FullName -ne $PSCommandPath }

# ── 替换规则（顺序敏感：长串必须排在短串之前）──────────────────────────────
# 用「扁平数组 + 步长 2」而不是嵌套数组/哈希表：
#   - 哈希表：PowerShell 的键默认大小写不敏感，`wb-switch` 与 `WB-SWITCH` 会判定为
#     重复键直接报错；
#   - 嵌套数组：`@(@('a','b'), @('c','d'))` 会被 `@()` 展平成一维，`$pair[0]` 退化成
#     单个字符——本脚本第一版就踩了这个坑，表现为「把 W 替换成 B」这种灾难性结果。
# 字符串的 .Replace() 是大小写敏感的，正是这里需要的语义。
$rulePairs = @(
    # 环境变量
    'WB_SWITCH_GATEWAY_BIN',         'AI_GATEWAY_ROUTER_BIN'
    'WB_SWITCH_SCREENSHOT_DEMO',     'AI_GATEWAY_SCREENSHOT_DEMO'
    'WB_SWITCH_ACCOUNTS_FILE',       'AI_GATEWAY_ACCOUNTS_FILE'
    'WB_SWITCH_BINARY',              'AI_GATEWAY_BINARY'
    'WB_SWITCH_HOME',                'AI_GATEWAY_HOME'
    'WB_PROBE_TIMEOUT_SEC',          'AI_GATEWAY_PROBE_TIMEOUT_SEC'
    # Rust 标识符（下划线形）
    'wb_switch_rust_lib',            'ai_gateway_lib'
    'wb_switch_core',                'ai_gateway_core'
    'wb_switch_gateway',             'ai_gateway_router'
    'wb_switch_test_',               'ai_gateway_test_'
    'wb_switch',                     'ai_gateway'
    # crate / 包名（连字符形）——长名优先
    'workbuddy-switch-gateway',      'ai-gateway'
    'wb-switch-core',                'ai-gateway-core'
    'wb-switch-gateway',             'ai-gateway-router'
    'wb-switch-server',              'ai-gateway-server'
    'wb-switch-rust',                'ai-gateway'
    'workbuddy-switch',              'ai-gateway'
    'wb-switch',                     'ai-gateway'
    # 大写/显示名形态
    'WB-SWITCH',                     'AI-GATEWAY'
    'WorkBuddy Switch Gateway',      'AI Gateway'
    'WorkBuddy_Switch_Gateway',      'AI_Gateway'
    'WorkBuddy.Switch.Gateway',      'AI.Gateway'
    'com.momo0410.wbswitch.gateway', 'com.momo0410.aigateway'
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

