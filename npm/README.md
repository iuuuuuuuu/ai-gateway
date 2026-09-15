# workbuddy-switch

WorkBuddy（腾讯 AI 编程助手）账号切换工具，支持 Windows x64、macOS（Apple Silicon / Intel）与 Linux x64。两种形态：

- **桌面 App**：下载对应平台安装包运行（Tauri，推荐日常使用）
- **npm / webui**：`npm i -g workbuddy-switch` 后运行 `workbuddy-switch`，浏览器打开操作界面

多账号共享登录态（`workbuddy-desktop.info`），一键切换 WorkBuddy 登录账号，并支持将当前账号的会话复制给目标账号（云端归属目标）。

## 快速开始

### npm 安装（webui）

```bash
npm i -g workbuddy-switch
workbuddy-switch              # 启动本地服务 + 自动打开浏览器
workbuddy-switch status       # 终端查看当前账号
```

webui 界面与桌面 App 一致：账号管理、切换、会话复制、自动签到、token 保活、更新检查。

> 本包通过**平台子包**分发二进制（`workbuddy-switch-<platform>-<arch>`）：它们声明为
> `optionalDependencies`，安装时由 npm 按本机 `os`/`cpu` 自动挑一个装好，postinstall
> 再从平台包复制二进制到 `bin/`，**不依赖 GitHub**，国内镜像（npmmirror）也能稳定安装。
> 目前提供 `win32-x64`、`darwin-arm64`、`darwin-x64`、`linux-x64`、`linux-arm64` 五个平台包。

### 桌面 App

从 GitHub Releases 下载对应平台的安装包：

| 平台 | 产物 |
|---|---|
| Windows x64 | `*_x64-setup.exe` / `*.msi` |
| macOS（Apple Silicon） | `workbuddy-switch_<版本>_aarch64.dmg` |
| macOS（Intel） | `workbuddy-switch_<版本>_x64.dmg` |
| Linux x64 | `*.deb` / `*.AppImage` |

> macOS 产物为 adhoc 签名（无 Apple 开发者证书），首次打开若提示「已损坏」，
> 执行 `xattr -cr "/Applications/WorkBuddy Switch Gateway.app"` 放行即可。

## 功能

| 模块 | 说明 |
| --- | --- |
| 账号管理 | OAuth 扫码登录、从本机导入（含历史登录快照与切换备份）、删除账号 |
| 账号切换 | 备份认证文件 → 关闭 WorkBuddy → 写入目标账号 → 重启，切换过程实时进度反馈 |
| 会话复制 | 将当前账号勾选的会话以新 id 复制给目标账号（jsonl 正文 + `workbuddy.db` 索引 + edge-sync 注册） |
| 自动签到 | 默认开启；启动时立即检查，运行期间每 30 分钟自动补签；一键全部签到；30 天签到日志 |
| Token 保活 | 惰性刷新（操作前不足阈值刷新）+ 每日保活（默认每天无条件刷新一次，阈值 >0 时仅刷新剩余不足该天数的账号），避免 refresh token 过期 |
| 自动更新 | 配置 GitHub Releases 源检查新版本；整包更新经签名校验（tauri-updater） |

## 使用

1. **添加账号**：账号页 →「扫码登录」（OAuth device flow）或「从本机导入」（扫描当前登录态、
   历史登录快照与切换备份，可一次勾选多个账号）「手动添加」
2. **切换账号**：账号卡片 →「切换」，可勾选复制当前会话
3. **自动签到**：账号页可直接开关；设置页可调整保活参数、立即签到并查看日志
4. **更新**：设置 → 自动更新，填写 GitHub owner/repo/token 后检查更新

## 许可

[MIT](./LICENSE)
