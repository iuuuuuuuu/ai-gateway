## AI Gateway 1.0.3 · 修复 Token 统计的模型名归属

本版修掉 Token 统计里两处模型名显示错误。本机实测：**CodeBuddy IDE 来源的 88 条
记录此前全部显示「未知模型」，现在能正确显示真实模型名。**

### 修复一：经代理使用 Claude Code 时，显示真实后端模型

把 Claude Code 通过代理接到别的模型上时（ocgo、自建网关等），harness 请求的仍是
`claude-opus-5` / `claude-sonnet-5` / `claude-haiku-4-5`，而后端实际应答的是被映射
过去的模型。会话日志把两者都记了下来：

```
"model": "glm-5.2",  "requestModelId": "claude-opus-5"
```

`model` 是**真正应答本次请求的模型**，`requestModelId` 是客户端请求的名字。
旧代码只读了 `providerData.model`，一旦该字段缺失或为空就直接显示「未知模型」。
现在按 `model` → `requestModelName` → `requestModelId` 取值，**且绝不倒置该顺序** ——
`requestModelId` 装的是 `auto` / `balanced-model` 这类路由档位名，优先采用会把真实
模型名抹平成「Auto」，正是要避免的失真。

### 修复二：CodeBuddy IDE 来源不再全部显示「未知模型」

新版 IDE 把模型存在 `modelMap` 里，按会话类型分槽位，例如
`{"craft": "kimi-k2.6"}`。旧代码只找平铺的 `selectedModelId` / `modelId` / `model`，
一个都命中不了，于是 IDE 来源的每一条都落到「未知模型」。

现在先按会话自身的 `type` 读 `modelMap`，再依次退到 `craft`、任意槽位、平铺字段。
本机实测该来源由 **88 条未知模型** 变为正确显示 `kimi-k2.5` / `kimi-k2.6` / `glm-5.1`。

### 顺带说明：`custom-local:` 前缀现在会被保留

本地自定义模型在日志里写作 `custom-local:deepseek-v4-flash`（带 provider 前缀），
旧代码会把这个前缀丢掉，等于把「本地自定义模型」和「同名官方模型」静默合并成同一行。
现在两者分开统计。**各模型底层的 token 计数没有变化，只是标签变得准确** ——
如果你此前对比过总数，会发现某些行的数字「变小了」，那是因为它被拆成了两行。

### 一处仍然显示「未知模型」的情况

有极少数记录（本机为 7 条）的 `providerData` 里**只有** `usage` 块，记录的任何位置
都没有模型字段。这类记录没有任何可归属的模型信息，仍显示「未知模型」。

### 安装

本版在本地构建签名（私钥不经过 CI），仅提供 Windows 安装包。

| 平台 | 文件 |
|---|---|
| Windows | `ai-gateway-windows-x86_64-setup.exe` |

updater 清单 `latest-windows-x86_64.json` 随本 Release 一并提供，
签名 keyid `41d821432b0e7c21` 与 `tauri.conf.json` 的公钥配对（已验签）。
