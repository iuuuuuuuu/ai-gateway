## AI Gateway 1.0.4 · 修复流式输出「夹断且无报错」

本版修掉一个会让回答**莫名其妙断在半截、而且没有任何报错**的网关缺陷。
如果你遇到过「问一个问题，回答到一半停了，看日志也没报错，只能再发一条
才会继续」——就是这个 bug。建议所有用户升级。

### 现象与原因

网关的两个流式接口在检测到「上游中途断流」时，都会**先报失败、再补一个
成功收尾**。而客户端只认最后一个事件，于是截断的半截回复被当成完整回答
收下 —— 所以你看到的是「断了，但没报错」。

`/v1/responses`（Codex / DeepSeek Harness 走这条）实测事件序列：

```
response.created
response.output_item.added
response.output_text.delta
response.failed            ← 已经报失败了
response.output_text.done  ┐
response.output_item.done  ├ 但收尾照常执行
response.completed         ┘ ← 客户端认这个，判定成功
```

`/v1/messages`（Claude Code 走这条）更严重：**连 error 事件都不发**，
断流后照常发 `message_stop`，而它在 Anthropic 协议里就表示「本轮正常结束」。

三个客户端（Codex、DSH、Claude Code）受同一个根因影响，只是走的事件通道不同。

### 什么时候会触发

上游流中途断掉就会触发。以下任一情况都会导致断流：

| 触发源 | 默认值 |
|---|---|
| 流中静默超时 | 300s |
| 首字节超时 | 120s |
| 并发满时换号 | `max_in_flight` 3 |

模型思考时间较长时（思考期间不吐 token）容易吃满空闲超时，所以表现为
「时好时坏、没有固定规律」。

### 修复方式

失败路径直接结束本轮，不再往下执行成功收尾。上游正常结束靠的是 EOF
（发完 `[DONE]` 后关连接），这条路径仍走成功收尾 —— 特意用独立测试锁住，
避免修完变成「所有正常回复都报错」。

修复后，上游断流会明确报错，你能看到真实原因；不再静默吞掉半截回复。

### 安装

四平台照常发布。

| 平台 | 文件 |
|---|---|
| Windows | `ai-gateway-windows-x86_64-setup.exe`、`.msi`、`portable.zip` |
| macOS (Apple Silicon) | `ai-gateway_1.0.4_aarch64.dmg` |
| Linux | `AI.Gateway_1.0.4_amd64.deb`、`.AppImage` |

updater 清单 `latest-windows-x86_64.json` 随本 Release 一并提供，
签名 keyid `41d821432b0e7c21` 与 `tauri.conf.json` 的公钥配对（已验签）。
