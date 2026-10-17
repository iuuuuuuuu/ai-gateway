# Agent Note: 发版前必须过测试门控：preflight 用 needs 拦住发布

Status: implemented

## Problem

原 `docs/DECISIONS.md` D16（从 DECISIONS.md 迁移）。

`release.yml` 的注释原话：

> 本工作流此前**没有任何测试** —— 没有 cargo test、没有 cargo check、没有网关单测，
> 唯一的校验是产物断言（`.sig` 数量、安装包存在性）。也就是说，
> **一个 tag 完全可以为「从未通过编译校验与单测」的代码发布安装包，
> 而且发布一旦出去就撤不回来。** 这一点在 push 阶段由 `ci.yml` 覆盖，
> 但打 tag 时并不强制要求 ci 绿 —— **tag 可以打在任意 commit 上**。

即：`ci.yml` 绿不绿，与「能不能发版」**没有任何关系**。

## Decision

`release.yml` 增加 **preflight** job（单个 ubuntu job）作为发版前的验证门，
四个平台 build job **必须等它通过**才开跑。

用 `needs`（**能拦住**），**不用** `continue-on-error`。

### 为什么是一个独立 job 而不是塞进四个平台 job

那会把同样的检查重复跑 4 遍，而 windows 那条关键路径已经很贵。
用一个 ubuntu job 做门控，四平台等它的代价只有 ~2 分钟，
换来「**发布出去的必定测过**」。

### ★ 与 build 的策略是相反的，两者互不冲突（最容易改错的地方）

| job | 策略 | 目的 |
|---|---|---|
| **preflight** | `needs` | **就是要它能拦住**发布 |
| **build** | `continue-on-error: true` + release job 的 `!cancelled()` | **单平台失败不阻断**其余平台，也不阻断 Release |

⚠ 两个细节缺一不可：`continue-on-error` 只保证「本 job 失败不把整个 run 判为失败」；
**release job 必须配合 `!cancelled()`** 才会在 build 失败时照常执行 ——
因为 `needs: build` 默认要求依赖成功。只加 `continue-on-error` 而不改 release 的 `if`，
仍然会出现「一个平台挂了、整个 Release 一个资产都发不出来」（**v0.7.1 就是这样**）。

### ★ preflight 自己也有一处必须留意的边界

`!cancelled()` 对「**被 skip**」也是 true ⇒ 必须同时判
`needs.preflight.result == 'success'`，否则 preflight 一挂、build 被 skip、
release 照常执行却一个产物都没有，轻则报错重则**发一个空 Release**。

## Alternatives considered

- **不设门控（维持现状）** — 它最强的理由是**发版最快**：tag 一推就走完，
  不需要多等 ~2 分钟的 preflight，也不会因为 preflight 环境问题（网络、依赖缓存）
  卡住一次本可以成功的发布。否掉的原因是它保留着「tag 可以打在任意 commit 上」
  这个漏洞：一个从未通过编译校验的 commit 可以发布安装包，
  而且发布出去撤不回来。~2 分钟换「发布出去的必定测过」，这笔账很清楚。

- **把检查塞进每个平台 job** — 它最强的理由是**就近校验**：每个平台验自己的产物，
  不必在一个 ubuntu job 里假装能代表四个平台，语义上更「正确」。
  否掉的原因是 **4 倍成本**：同样的检查跑 4 遍，而 windows 那条是关键路径，
  每个 job 都加一遍会让总时长显著上升，却不会多发现任何问题
  （那些检查与平台无关的部分完全重复）。

## Consequences

- **收益**：发版前强制经过编译校验与单测，「发出去的必定测过」。
- **收益**：preflight 与 build 的**相反策略**被明确记录 —— 这一点极易被「统一」掉。
- **代价与已知上限**：发版多约 2 分钟前置时间；多一个可能因环境问题失败的 job。
  **信号**：若出现「preflight 挂了但 Release 还是发出去了」，说明
  `needs.preflight.result == 'success'` 那条判断被删了 —— 那会**发空 Release**。
- **代价**：**门控的有效性依赖「tag 打在已通过 CI 的 commit 上」这条纪律** ——
  1.0.37 的 tag 就打在 CI 红的那次（其 release run 的 build / 发布两步是 skipped，
  Release 是手工建的）。⇒ **发版前先看 CI 是否绿**。
  这条无法用流水线自身强制（tag 可以打在任意 commit 上），只能靠纪律。

## Verification

- `release.yml` 里 preflight job 存在，且四个 build job 都有 `needs: preflight`。
- release job 的 `if` 必须同时包含 `!cancelled()` 与
  `needs.preflight.result == 'success'`。
- build job 带 `continue-on-error: true`（单平台失败不阻断其余平台）。
- **不翻案**，但上述「tag 纪律」无法机械强制 —— 见 Consequences 的代价。
