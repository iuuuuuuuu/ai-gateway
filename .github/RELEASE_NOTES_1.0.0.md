## AI Gateway 1.0.0 · 首个正式版

从 0.8.5 到 1.0.0 是一次大版本跃迁：项目更名，并从「WorkBuddy / CodeBuddy 双应用」
扩展为**五应用一体化工作台**。

### 项目更名

`WorkBuddy Switch Gateway` → **AI Gateway**。crate 名、应用标识、数据目录、安装包名
全量同步；旧数据在首次启动时**自动迁移**（复制式、旧目录保留、新旧两版可并存）。

### 新增：Trae 账号管理（Trae Work + Trae）

- **登录态切换**：15 项白名单快照 + 单代回滚；恢复前删除 WAL 边车，避免旧账号被回放
- **一键签到**：`status` 预检 → `claim` → 错误分类 → 冷却落盘；积分归属三层兜底
- **设备指纹隔离**：按 uid 确定性派生 `device_id` / `session_id` / `market_user_id`
- **6 层设备标识重置**：machineid / 遥测 / aha / TinyStorage / 注册表 MachineGuid / webview
- **本机账号发现**：识别账户中心与 Cloud-IDE 两套编号体系，推导失败时拒绝入池

### 新增：豆包账号管理

- **抓包凭证回写**：按抓包文件自身的 uid 定位，绝不自动建号，内容未变不写盘
- **多 Profile 快照**：版本校验 + 单代回滚 + 活跃 Profile 指针修复
- **会话保活**：启动客户端触发服务端 30 天滑动续期（客户端有第二层加密，离线取不到明文）
- **会员额度**：精确解析 + 宽容兜底；支持单账号查询与全量巡检
- **对话备份与导出**：客户端状态备份/恢复，官方 IM API 导出 markdown + json

### 新增：本地 MITM 设备代理

自签 CA、流量拦截、凭证捕获、系统代理原样还原、崩溃看门狗、WebSocket 观测。
以 hyper 自建而非用 hudsucker —— 后者无法透传用户 VPN 上游。

### 新增：计划任务与 CLI 任务模式

Windows 计划任务双轨（应用内调度 + 系统计划任务），`--task-run` 刻意不启动 Tauri
（单实例插件会把计划任务触发误判成重复启动而静默退出）。

### 智能体一键接入扩展到 12 类

新增 **MiniMax Code**（Anthropic Messages 协议）。接入时保留其官方 provider
登录态，官方账号与本网关可共存切换。

### 修复

- **上下文超长不再伪装成账号故障**，也不再对着每个账号重传（合入上游 PR #17）
- 数据目录迁移曾被永久阻断（演示模式写入不完整账号库后，真实账号再也搬不过来）
- 账号刷新导致重复条目：`upsert_account` 只按 `id` 匹配，无 `id` 的条目每次刷新都被追加
- 三处跨平台缺陷：Windows 路径识别、icube 快照路径拼接、代理套接字阻塞标志

### 安装

| 平台 | 文件 |
|---|---|
| Windows | `AI.Gateway_1.0.0_x64-setup.exe`（推荐）/ `.msi` / 便携版 zip |
| macOS | `ai-gateway_1.0.0_aarch64.dmg`（Apple Silicon）/ `_x64.dmg`（Intel） |
| Linux | `.AppImage` / `.deb` |

> macOS 为 adhoc 签名，首次打开若提示「已损坏」，执行
> `xattr -cr "/Applications/AI Gateway.app"` 放行。
