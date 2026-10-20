# Agent Note: 宿主把 `listen` 重写成 `:port`——手改 0.0.0.0 永远不生效

Status: implemented

## Problem

业主现场（2026-10-20）：

> 「还有个问题，现在又不能监听 0.0.0.0 了，你看看是怎么回事」

运行中的网关日志是 `listening on http://127.0.0.1:7864`，而业主需要局域网设备访问。

### 根因：`finalize_gateway_config` 每次都用 `:port` 覆盖 `listen`

```text
宿主 finalize:  cfg["listen"] = normalize_listen(port)   // 恒为 ":7864"
网关 normalize: ensure_listen_host(":7864") = "127.0.0.1:7864"  // 安全默认
```

`ai-gateway-router` 的 `ensure_listen_host` 把空 host 归一成回环是 **2026-10-17
的刻意修复**（Windows 上空 host 会 bind 到 IPv6 link-local，宿主探活 127.0.0.1
必然超时）。但它与宿主的 `:port` 配合后产生了一个副作用：

- 用户在 `gateway_config.json` 里手写 `"listen": "0.0.0.0:7864"`；
- 下一次读取/保存配置时，`finalize_gateway_config` 把它覆盖回 `:7864`；
- 网关再归一成 `127.0.0.1:7864`。

⇒ **手改 0.0.0.0 在结构上不可能生效**，而且界面上没有任何开关能表达这个意图。

## Decision

新增显式配置键 `lan_access`（bool，默认 `false`），由它决定 `listen`：

| `lan_access` | `listen` |
|---|---|
| `true` | `0.0.0.0:{port}` |
| `false` | `127.0.0.1:{port}` |
| 缺键 + 手写非回环 host | **原样保留**（兼容老配置，并推断为 `true`） |
| 缺键 + 空 host / 裸端口 / 回环 | `127.0.0.1:{port}`（安全默认） |

解析逻辑集中在 `resolve_listen(cfg, port)`，**三处调用点共用**：
`finalize_gateway_config`（读/存配置）、`resync_native_config`（账号刷新后重写
native config）、`start_gateway`（启动前重写）。此前三处各写一遍
`normalize_listen(port)`，任何一处漏改都会让 0.0.0.0「时灵时不灵」。

### 老配置兼容必须发生在 overlay(defaults) **之前**

`default_gateway_config()` 现在带 `lan_access: false`。若直接把磁盘配置 overlay
在默认值上，缺键的老配置会变成「显式 false」，手写的 `0.0.0.0` 仍会被改回回环。
故 `merge_gateway_config` 在合并前先看**原始磁盘配置与本次 patch**：缺
`lan_access` 且有非回环、非空 host 时补 `true`。显式 `false` 的对象不动 ——
那才是用户明确要回环。

### 界面

网关页「服务端口」下方新增「允许局域网访问」开关：
`save_gateway_config` 的 Tauri 参数（camelCase `lanAccess`）、WebUI 的
`normalize_gateway_config_body` 别名表、`src/lib/api.ts` 的
`saveGatewayConfig` 转发三处**同步补齐**。任何一处漏掉，保存都会返回成功但字段
被静默丢弃（Tauri 的 serde 与 WebUI 的归一化都会这样）。

## Alternatives considered

- **直接把默认值改成 `0.0.0.0`** — 最强理由是**零 UI 改动**，老用户升级后
  自动恢复局域网访问。否掉的原因是**安全默认**：`api_key` 默认为空（= 不鉴权），
  默认监听全网卡等于把账号额度暴露给整个局域网；且这会重新引入
  `:port` 与 `0.0.0.0` 的语义混淆（空 host 在 Windows 上不可控）。

- **只保留手写的 `listen`，不加 `lan_access`** — 最强理由是**改一处**：
  `finalize` 检测到非回环 host 就原样保留。否掉的原因是**界面无法表达**：
  用户点「关闭」时没有任何字段能覆盖一个手写的 `0.0.0.0`，只能去手改 JSON；
  而且「读配置时推断」与「写配置时保留」会形成两份真相。

- **加 `listen_host` 字符串而不是 bool** — 最强理由是能表达「绑到某个具体
  网卡 IP」。否掉的原因是**界面复杂度**：本次需求只有「本机 / 局域网」两态，
  字符串还要做校验、空值、非法值回退；而手写具体 IP 的老配置仍会被
  `resolve_listen` 原样保留（只在用户用 UI 改成 LAN 时归一成 0.0.0.0）。

- **保存后自动重启网关** — 最强理由是「开关立即生效」。否掉的原因是**会掐断
  正在进行的对话**（与端口修改的既有口径一致：提示「重启后生效」）；用户正在
  用网关时自动重启是不可接受的副作用。

## Consequences

- **收益**：`0.0.0.0` 现在既能通过界面开关设置，也能由手写配置保留；
  三处覆盖点共用同一个解析函数，不会再出现「保存后好了、重启又变回去」。
- **收益**：默认仍是回环，行为对老配置逐字不变（缺键 + `:port` ⇒ 127.0.0.1）。
- **代价**：新增一个配置键与一个界面开关；`gateway_config.json` 会多出
  `lan_access`，`listen` 变成显式 host（不再以 `:port` 存储）。
- **已知上限**：手写的**具体网卡 IP** 在用户用界面切换一次后会归一成
  `0.0.0.0`（绑定范围可能变宽）。已在代码注释里写明；需要固定网卡 IP 的部署
  应直接维护 native config 并关闭 UI 的 LAN 开关。

## Verification

单测：

- `ai-gateway-core`：`finalize_keeps_manual_wildcard_listen` /
  `finalize_preserves_specific_manual_host` /
  `finalize_lan_access_controls_listen_host` / `finalize_normalizes_empty_host_to_loopback` /
  `finalize_fills_listen_default`（全绿；core 共 1306 passed）。
- `ai-gateway-router`：1846 passed（`ensure_listen_host` 原有用例不变）。
- 前端 `npm run build`（tsc + vite）通过。

实机（隔离实例 57899，未触碰业主 :7864/:3080）：

```text
config: "listen": "0.0.0.0:57899"
日志:   rust gateway listening on http://0.0.0.0:57899
netstat: LocalAddress=0.0.0.0  LocalPort=57899  State=Listen
```

## 后续（未做）

- 宿主目前没有任何「重启网关」按钮会**重新计算**监听地址：改完开关后需要
  「停止 → 启动」或重启应用。启动路径已走 `resolve_listen`，故重启后一定按新值
  监听；这是与端口修改一致的既有口径。
