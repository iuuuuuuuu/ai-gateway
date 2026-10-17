# Agent Note: 空 host 的 listen（`:7863`）在 Windows 上不监听回环，导致网关启动超时

Status: implemented

## Problem

v1.0.47 的用户装完打不开，报：

```
网关启动超时（端口 7863）：error sending request for url (http://127.0.0.1:7863/healthz)
```

我们自己的机器上没更新到那一版，所以这是**只在全新安装上出现**的回归 —— 而 CI 在
`ubuntu-22.04` / `macos-14` / `windows-latest` 三腿全绿。三腿全绿与用户全挂能
同时成立，说明测试没覆盖到真正出问题的那条路径。

### 成因链条（每一环都有实测）

v1.0.47 把网关的 bind 从写死回环改成「配置里的 host 必须生效」：

```rust
// v1.0.46
let listener = TcpListener::bind(("127.0.0.1", port)).await …;
// v1.0.47
let listener = TcpListener::bind(&bind_addr).await …;   // bind_addr = cfg.listen
```

而宿主写进配置的 `listen` **恒为空 host**：`normalize_listen()` 返回
`format!(":{port}")`，落盘就是 `":7863"`。网关侧的护栏当时写的是

```rust
if !self.listen.contains(':') {                       // 只处理无冒号的裸端口
    self.listen = format!("127.0.0.1:{}", self.listen);
}
```

`":7863"` **含冒号** ⇒ 不触发 ⇒ 原样交给 `bind`。

空 host 交给 `TcpListener::bind` 后走 `getaddrinfo`，**各平台结果不同**（本机实测）：

```
to_socket_addrs(":7863") 解析出 9 个地址，第 0 个是
  [fe80::e7b7:916d:77c6:472d%6]:7863      ← IPv6 link-local
bind(":0") -> local_addr = [fe80::…%6]:60672
连 127.0.0.1:60672 -> ERR connection timed out   ← 宿主探活必然失败
```

- **Linux / macOS** → 空 host 解析成回环，`127.0.0.1` 能通 ⇒ **CI 全绿**
- **Windows** → 枚举全部网卡，IPv6 link-local 排第一 ⇒ **不监听 `127.0.0.1`**

宿主探活打的是 `http://127.0.0.1:{port}/healthz`，且启动循环里 `probe_health`
返回 `Err` 时只是记 `last_err` 继续轮询，**唯一**的提前退出判据是子进程已退出
（`child.try_wait()`）。网关并没有退出 —— 它绑成功了，只是绑错了地址 ——
所以宿主完整空转 20 秒，最后抛出那句超时。

不修的话，**所有 Windows 用户的全新安装都起不来**，而且日志里那句
`listening on http://:7863` 看起来不像错误，排查方向会被带偏（甚至会怀疑端口被占）。

## Decision

判据从「没有冒号」改为「**host 为空**」，并归一成回环 —— 保持 v1.0.46 的实际行为。

归一逻辑收在**一个**函数里，`crates/ai-gateway-router/src/config.rs` 的
`ensure_listen_host(&str) -> String`，三处消费方共用：

| 输入 | 输出 | 说明 |
|---|---|---|
| `":7863"` | `"127.0.0.1:7863"` | ★ 宿主实际写入的形态 |
| `"7863"` | `"127.0.0.1:7863"` | 裸端口（旧判据已覆盖，不许回退） |
| `""` / `"   "` | `"127.0.0.1:7863"` | 空白回落默认端口 |
| `"127.0.0.1:"` | `"127.0.0.1:7863"` | 只有冒号没端口 |
| `"[]:7863"` | `"127.0.0.1:7863"` | IPv6 空 host |
| `"0.0.0.0:7863"` | `"0.0.0.0:7863"` | **原样保留** —— 部署形态必须生效 |
| `"[::1]:7863"` | `"[::1]:7863"` | IPv6 字面量，方括号里的冒号不是分隔符 |

正确性先判 `is_ipv6`（以 `[` 开头），再 `split_once(':')` —— 否则
`[::1]:7863` 会在第一个冒号处被切断。

两处消费方：

1. `Config::normalize()` —— 配置解析后立刻归一，`from_json` / `load` 都会经过。
2. `bin/gateway.rs` 构造 `bind_addr` 时**再兜一次**。理由不是备份，是
   `cfg.listen` 还有 `WB2A_LISTEN` 环境变量这条注入路径（`config.rs::apply_env`），
   入口不止一个；且这类失败的形态是「静默绑到错地址 + 打印看起来正常的日志」，
   宁可多一次防御。

`crates/ai-gateway-core` 的 `normalize_listen()` **返回值不变**（仍是 `":{port}"`）——
`port_of()` 与多处既有测试都按这个形态断言，真正要修的是消费方对空 host 的解释。
已在它的 doc 注释里写明这层契约与那个「别改成 `127.0.0.1:{port}`」的警告。

## Alternatives considered

- **把 `normalize_listen()` 改成返回 `"127.0.0.1:{port}"`** — 它最强的理由是
  「从源头消除歧义」：配置里存的就是完整地址，网关不必猜，将来任何消费方都不会
  再踩同一个坑。否掉的原因是**爆炸半径**：`port_of()` 按 `:port` 形态解析，
  `gateway.rs` 里至少 10 处测试硬断言 `":7863"` / `":7864"` 这类值，
  改返回值要连带改一大片断言，而那些断言本来测的是别的事（配置合并、运行态不覆盖等）。
  一处消费方归一 vs 全仓改断言，前者代价小得多。

- **让网关监听 `0.0.0.0`（把空 host 当作「所有网卡」，即 Go 的语义）** — 它最强的
  理由是**语义一致**：`:7863` 在 Go 时代就是全网卡，照此解释不算改动行为，
  且「部署到服务器要被外部访问」的场景确实需要它。否掉的原因是**这是安全倒退**：
  `api_key` 默认为空（= 不鉴权），监听全网卡等于把本机账号额度暴露给整个局域网，
  `/status` 还会带出账号标识。而 v1.0.46 的实际行为（写死 `127.0.0.1`）就是只回环
  —— 按 Go 语义解释反而是**擅自改变用户已有的暴露面**。要对外暴露时应显式写
  `0.0.0.0:{port}`，那时网关会打非回环告警。

- **只修 `Config::normalize()`，bin 侧保持直接用 `cfg.listen`** — 它最强的理由是
  「单一真源」：归一只有一处，不存在两处判据漂移的可能。否掉的原因是
  `apply_env` 会绕过配置文件的解析路径注入 `listen`，而兜底失败的形态是静默的
  （绑到错地址、日志还打印正常地址）。两处都调**同一个函数**，不存在判据漂移，
  只是调用点多一个。

- **在启动循环里加「绑错地址」的检测**（例如比对日志里的地址） — 它最强的理由是
  「在下游拦住，不必改上游语义」。否掉的原因是它治标：`bind` 成功但地址不对这件事
  没有任何可靠的运行时信号可判（要解析自己的日志），而把空 host 归一才是根因所在。

## Consequences

- **收益**：Windows 全新安装恢复可用。A/B 实测（同一份 `listen=":57897"` 配置）：
  修复前日志 `listening on http://:57897`、`curl` 退出码 **7（连接被拒绝）**；
  修复后日志 `listening on http://127.0.0.1:57897`、`curl` 退出码 **0** 且
  `x-service: workbuddy2api` ⇒ 宿主 `probe_health` 走 `Ok(v)` + `identity_ok=true`
  ⇒ `started=true`。
- **收益**：顺手修掉一处**误报** —— 修复前日志会打
  `⚠ 网关正在监听 :57897（非回环）但 api_key 为空`，把回环绑定误报成对公网开放。
  归一之后这段告警只在真的非回环时出现。
- **代价与已知上限**：`ensure_listen_host` 是一层「猜意图」的归一，不是严格的地址
  校验 —— 它不验证端口是否合法、也不做 DNS 解析，非法端口仍要等 `bind` 报错。
  **信号**：若将来出现「配置里明明写了 `0.0.0.0:port` 却只听回环」，
  或反过来「只想回环却被绑到全网卡」，说明这层归一被改坏了 —— 那两条都已有测试钉住。
- **代价**：`listen` 现在有三个可能形态（裸端口 / 空 host / 完整地址）在流转，
  契约靠注释与测试维持，没有类型层面的强制。

## Verification

三个单元测试（`crates/ai-gateway-router/src/config.rs`）：

| 测试 | 钉住什么 |
|---|---|
| `listen_with_empty_host_becomes_loopback` | `:7863` / `7863` / `""` / `"   "` / `"127.0.0.1:"` 全部归一成回环 |
| `listen_with_explicit_host_is_preserved` | `0.0.0.0` / `192.168.x` / `localhost` / `[::1]` / `[::]` / `[]:` —— 显式 host 不许被改写 |
| `config_from_json_normalizes_empty_host_listen` | 走完整 `Config::from_json` 路径，证明 `normalize()` **真的调到了**归一函数 |

★ 第三条是刻意加的：光测 `ensure_listen_host` 本身证明不了它被接进了解析链 ——
**测试够不着被改的代码**这个坑本仓库已经踩过一次（见
`2026-10-16-banned-11140-not-3012.md` 的「第一版修复是假绿门禁」）。

**故障注入（已做，确认会红）**：把 `normalize()` 里那行还原成旧判据
`if !self.listen.contains(':')` ⇒ `config_from_json_normalizes_empty_host_listen`
立刻失败，报错原文：

```
assertion `left == right` failed: 宿主写入的空 host 必须在 normalize 阶段补成回环
  left: ":7863"
 right: "127.0.0.1:7863"
```

撤销后复绿。`left` 正是缺陷值，说明这条门禁打在真路径上。

**端到端实测（按宿主的判据，不是只看 HTTP 200）**：宿主 `probe_health` 用
`reqwest`，**503 也是 `Ok(resp)`**，成败取决于 `x-service` 头 / `service` 字段。
所以验证不能只看状态码 —— 用 `curl -i` 取原始响应，按
`probe_health` 的判据复算 `identity_ok`：

```
HTTP/1.1 503 Service Unavailable
x-service: workbuddy2api
{"healthy":0,"service":"workbuddy2api","total":0}
```

503 是因为隔离实例 `accounts=0`，属正常；`identity_ok=true`，
宿主据此判 `started=true`。
