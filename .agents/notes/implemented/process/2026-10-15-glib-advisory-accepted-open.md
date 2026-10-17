# Agent Note: 已知并接受的依赖告警：glib 0.18.5 不可升级，保持 open

Status: implemented

## Problem

原 `docs/DECISIONS.md` D17（从 DECISIONS.md 迁移）。

`Cargo.lock` 里的 `glib 0.18.5` 命中 **GHSA-wrw7-89jp-8q8g**
（medium，RustSec：`VariantStrIter` 的 `Iterator` / `DoubleEndedIterator` 实现不健全）。
GitHub 依赖面板上它一直是红的。

**不能升级**，而且这个"不能"需要被记录 —— 否则每个看到告警的人都会重新排查一遍
依赖链，得到同样的结论，白烧时间。

## Decision

**当前无升级路径 ⇒ 接受该风险，告警在 GitHub 上保持 open（不 dismiss、不 suppression）。**

### 为什么会命中

`glib` 只经 **Linux** 依赖链进入：

```text
glib ← atk ← gtk 0.18.2 ← libappindicator 0.9.0 ← tray-icon 0.24.2 ← tauri 2.11.5
```

### 为什么升不动（三方同时钉死 `^0.18`；逐环核实）

| 环 | 约束 | 可否升 |
|---|---|---|
| `gtk 0.18.2` | `glib = "^0.18"` | `gtk` 最高只到 **0.19.0**（要求 `glib ^0.22`，本可修） |
| `libappindicator 0.9.0` | `gtk = "^0.18"` | 已是最高版 |
| `tray-icon 0.24.2` | `gtk = "^0.18"` | **最新 0.26.1 仍写 `gtk ^0.18`** |
| `glib` | 修复版从 **0.20.0** 起 | `0.18.x` 最高就是 0.18.5，**无 backport** |

可复现命令与输出：

```powershell
cargo update -p glib --precise 0.20.12
# error: failed to select a version for the requirement `glib = "^0.18"`
#   candidate versions found which didn't match: 0.20.12
#   required by package `gtk v0.18.2` ... of package `tauri v2.11.5`
```

### 为什么接受（两条把严重度压下来的事实，都是实测而非推断）

1. **Windows / macOS 目标根本不含 `glib`** —— `cargo tree -i glib --target
   x86_64-pc-windows-msvc` 与 `--target aarch64-apple-darwin` 都返回
   `warning: nothing to print`；**只有 `--target x86_64-unknown-linux-gnu`
   才拉进这条链**。本产品主力分发平台是 Windows，暴露面限于 Linux 产物。
2. **漏洞 API 在依赖图里不可达** —— 问题函数是 `VariantStrIter` /
   `Variant::array_iter_str()`；对整个已下载依赖源码
   `grep -rn 'VariantStrIter'`（排除 `glib` 自身定义）**外部调用点 = 0**；
   我们自己的代码 `grep -rn 'glib|gio::|VariantStrIter' src-tauri/src crates/*/src`
   **命中 = 0**。⇒ 没有任何路径会走到那个不健全的迭代器。

## Alternatives considered

- **dismiss 告警** — 它最强的理由是**让面板变干净**：一条永远修不掉的告警会让
  真正的新告警被淹没，而这条已经被逐环查死、确认不可达，留着它除了制造噪音
  没有任何作用。否掉的原因是 Linux 产物**仍在发布**
  （`AI.Gateway_*_amd64.deb` / `.AppImage`，见 v1.0.38 的 Release 资产）——
  dismiss 等于把「暴露面可能变化」这件事藏起来，而暴露面确实还存在。

- **自己 vendored 一份打过补丁的 `glib 0.18.5`** — 它最强的理由是**真正修掉**：
  改动只有 `&p` → `&mut p` 一处，技术上完全可行，修完告警就真绿了，
  也不必再写这么长一篇笔记解释为什么不管它。否掉的原因是**没有任何实测证据表明
  我们踩到了它**（见上「不可达」），而自维护一个补丁 crate 会引入长期维护面 ——
  每次 `glib` 上游变动都要重新 rebase 那个补丁。为一个不可达的缺陷引入长期维护面，
  不划算；**真出现真实崩溃再翻案**。

- **升 tauri 到 3.0 alpha** — 它最强的理由是**一次解决整条链**：新版本可能把
  `tray-icon` / `gtk` 一起带上去。否掉的是它的 `tray-icon ^0.25` 仍钉 `gtk ^0.18`，
  **不解决问题**，反而引入 alpha 风险。

## Consequences

- **收益**：这条告警的含义被写死，后来者不必重新排查依赖链。
- **收益**：明确了「不可达」的两条实测依据，而不是「大概没关系」。
- **代价**：面板上长期有一条 medium 告警，会与真告警混在一起。
  **信号**：若 `tray-icon` 或 `libappindicator` 换到 `gtk ^0.19` 及以上，
  直接升级即可修复，本条作废。
- **⚠ 维护纪律（最重要的一条）**：这条**故意保持 open**。它**不是**回归、
  **不是**待办、**不是**假绿门禁 —— **看到它时先读本条**，
  不要重复排查一遍依赖链（本次已把每一环查死）。

### 重访条件（任一成立就动手，此时优先「本地补丁」）

1. `tray-icon` 或 `libappindicator` 换到 `gtk ^0.19` 及以上 ⇒ 直接升级即可修复；
2. 有实测证据表明 Linux 产物真的走到了 `VariantStrIter`
   （如 NULL 解引用崩溃栈里出现该函数）；
3. 决定**停止发布 Linux 产物** ⇒ 该链彻底离开依赖图，可同时关掉这条告警。

## Verification

```powershell
# 1) 只有 linux target 才拉进 glib
cargo tree -i glib --target x86_64-pc-windows-msvc     # warning: nothing to print
cargo tree -i glib --target aarch64-apple-darwin       # warning: nothing to print
cargo tree -i glib --target x86_64-unknown-linux-gnu   # 有输出

# 2) 漏洞 API 不可达
#    对已下载依赖源码 grep VariantStrIter（排除 glib 自身定义）⇒ 外部调用点 0
#    对自己代码 grep 'glib|gio::|VariantStrIter' ⇒ 命中 0

# 3) 无升级路径
cargo update -p glib --precise 0.20.12                 # 报 ^0.18 约束冲突
```

另外两条同批告警**已经修好**（`source-map-js` → 1.2.2、`rustls` → 0.23.45），
不在本条范围内。
