## AI Gateway 1.0.2 · 拆分独立仓库 + 数据目录统一

本版把项目拆成两个独立仓库并行维护，并让两版**共用同一份账号数据**。
建议所有 1.x 用户升级到本版。

### 仓库拆分

| | 仓库 | 定位 |
|---|---|---|
| 本项目 | [`momo0410/ai-gateway`](https://github.com/momo0410/ai-gateway) | 1.x 主线（含网关图片能力、Trae/豆包等） |
| 姊妹项目 | [`momo0410/workbuddy-switch-gateway`](https://github.com/momo0410/workbuddy-switch-gateway) | 0.8.x 线，`main` 停在 v0.8.5 |

两仓库的 `releases/latest` 是**两个不同的指针**，因此更新端点必须各指自己。
本版把下列四处从 `workbuddy-switch-gateway` 改为 `ai-gateway`：

- `src-tauri/tauri.conf.json` 的 updater endpoints
- `crates/ai-gateway-core/src/modules/update.rs` 的 `GITHUB_REPO`
- `src/lib/update.ts` 的 `GITHUB_REPO`
- `scripts/gen-update-json.sh` 与 `publish-release.sh` 的默认 `REPO`

顺带修掉一处现在会帮倒忙的迁移逻辑：`load_github_config` 过去会把
`momo0410/workbuddy-switch-gateway` 自动迁到本仓库。老仓库现在是**合法的另一个
更新源**，再迁移会把「刻意留在 0.8.x 的用户」静默拉回 1.x。现在只迁移真正的上游
`changexbc/workbuddy-switch`。

### 数据目录统一回 `~/.wb-switch`

1.0.0 更名时把数据目录换成了 `~/.ai-gateway` 并做一次性迁移。由于本仓库此后与
老仓库并行维护、两版常同时装在一台机器上，那个迁移会让两边各自变成一份**快照**，
互相看不见对方新增的账号 —— 而从用户视角这是**同一个应用的两代**，理当共用数据。

本版把目录统一回 `~/.wb-switch`（0.8.x 一直在用、且仍在被写入的那个），
并**反转迁移方向**：首次启动会把 `~/.ai-gateway` 里的独有文件并进来。

一个必须说明的实现细节：**迁移标记换了新名字**（`.migrated-from-ai-gateway`）。
老标记 `.migrated-from-wb-switch` 写在当年的目标目录（`.ai-gateway`）里，而现在的
目标是 `.wb-switch` —— 若沿用同名，已跑过老迁移的机器会先被判为「已完成」，
但它的真实数据其实在 `.ai-gateway`，数据将永远搬不过来。

迁移特性：**复制式**（旧目录原样保留，可随时回退）、**并集合并**（账号按
`(区域, uid)` 去重，已有条目不被覆盖）、**只跑一次**、设了 `AI_GATEWAY_HOME`
时跳过。签名私钥的首选路径随之回到 `~/.wb-switch/wb-switch-updater.*`。

已在两个真实数据目录的完整副本上验证：`~/.wb-switch` 原有文件零丢失、零改写，
`~/.ai-gateway` 独有文件全部迁入，重复执行正确跳过。

### ⚠️ 已装 1.0.0 / 1.0.1 的用户需要手动升级

那两个版本的更新端点在源码里指向老仓库，**而端点写死在已发布的安装包里、改不了**。
它们现在检查更新会看到老仓库的 **v0.8.5**。**请不要点那个「更新」** ——
两个版本产品名不同（`WorkBuddy Switch Gateway` vs `AI Gateway`），NSIS 会判定为
不同应用，于是**再装一个 0.8.5**，机器上会出现三个应用。

请手动下载本版安装包覆盖安装一次；装好 1.0.2 之后自动更新即恢复正轨。

### 本版仅提供 Windows 安装包

本版在本地构建签名（私钥不经过 CI），只产出了 Windows nsis 包。
macOS / Linux 用户请等待后续版本，或在本地自行构建。

### 安装

| 平台 | 文件 |
|---|---|
| Windows | `ai-gateway-windows-x86_64-setup.exe` |

updater 清单 `latest-windows-x86_64.json` 随本 Release 一并提供，
签名 keyid `41d821432b0e7c21` 与 `tauri.conf.json` 的公钥配对（已验签）。
