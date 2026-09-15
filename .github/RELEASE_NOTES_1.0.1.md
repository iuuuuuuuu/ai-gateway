## AI Gateway 1.0.1 · 修复自动更新

1.0.0 的安装包**无法自动更新**，本版修复。建议所有 1.0.0 用户升级到本版。

### 问题

更名时把「应用显示名」与「GitHub 仓库名」当成同一件事，替换规则
`workbuddy-switch-gateway → ai-gateway` 连仓库 URL 一起改了，但仓库**当时并没有改名**。
于是下列四处全部指向并不存在的 `momo0410/ai-gateway`：

- `src-tauri/tauri.conf.json` 的 updater endpoints
- `crates/ai-gateway-core/src/modules/update.rs` 的 `GITHUB_REPO`
- `src/lib/update.ts` 的 `GITHUB_REPO`
- `scripts/gen-update-json.sh` 与 `publish-release.sh` 的默认 `REPO`

这类错误**构建期完全看不出来**：编译通过、测试通过、安装包正常，
只在用户点「检查更新」时静默 404。1.0.0 的 exe 里实际编进了 2 处失效 URL
（已用二进制字符串比对确认）。

### 修复内容

- 上述四处全部改回真实仓库名 `momo0410/workbuddy-switch-gateway`
- 新增 2 个单测把仓库名钉在一起，防止再次漂移：
  `repo_name_stays_in_sync_across_config_and_frontend`、
  `gen_update_json_default_repo_matches_constant`
  （已反向验证：故意改回错误值，单测确实失败）
- 顺带修掉同一批「过度替换」造成的两处隐蔽损伤：
  - **上游配置迁移失效**：判断条件被折叠成
    `repo == "ai-gateway" || repo == "ai-gateway"`（恒等重复），
    等于只剩一个分支，老用户配置里指向上游的 owner/repo 不会被迁移 ——
    表现为检查更新时提示上游版本，用户更新过去就会丢掉网关功能
  - 截图演示数据指向不存在的仓库，点「更新」会打开 404

### 关于 1.0.0 用户

1.0.0 的二进制里写死了失效地址，**它自己收不到本版更新**，需要手动下载本版安装包
覆盖安装一次；装好 1.0.1 之后自动更新即恢复正常。

从 0.8.5 及更早版本升级的用户不受影响：1.0.0 的更新清单已就地修正，
检查更新会直接提示 1.0.1。

### 安装

| 平台 | 文件 |
|---|---|
| Windows | `AI.Gateway_1.0.1_x64-setup.exe`（推荐）/ `.msi` / 便携版 zip |
| macOS | `ai-gateway_1.0.1_aarch64.dmg`（Apple Silicon）/ `_x64.dmg`（Intel） |
| Linux | `.AppImage` / `.deb` |

> macOS 为 adhoc 签名，首次打开若提示「已损坏」，执行
> `xattr -cr "/Applications/AI Gateway.app"` 放行。
