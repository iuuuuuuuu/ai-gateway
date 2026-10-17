# Agent Note: Go 参照实现留在仓库但退出构建链，交叉编译一并取消

Status: implemented

## Problem

原 `docs/DECISIONS.md` D02（从 DECISIONS.md 迁移）。网关内核重写为 Rust 之后
（见 [网关从 Go 全量重写为 Rust](2026-10-01-go-to-rust-gateway-rewrite.md)），
仓库里同时躺着两份实现，必须回答两个问题：

1. `go-gateway/` 是删还是留？删掉干净，但它里面有一份别人无法再生成的东西。
2. 构建链上的 Go 环节怎么收尾？只要还留着 `go build`，构建就会继续要求 Go 工具链，
   也就继续保留「本地没装 Go 就构建失败」这个失败面；而**交叉编译**又额外要求
   为目标三元组装 std 与链接器，是另一类独立的故障面。

不把这两件事一次性切断，就会出现「Rust 已经是主线，但构建仍被 Go 工具链牵制」的
半迁移状态 —— 这种状态最贵，因为两边都得维护，却两边都不完整。

## Decision

### `go-gateway/` 保留在仓库里，但不参与构建

`go-gateway/` **保留在仓库里**，但**不参与构建**。

### 构建链上三处都已切断

- `build-gateway.{sh,ps1}` 不再调用 `go build`；
- 三个 workflow 全部走 `sh scripts/build-gateway.sh`；
- `actions/setup-go` 从 `ci.yml` / `release.yml` 删除。

### 交叉编译同时取消

**交叉编译同时取消**：脚本收到 `GOOS`/`GOARCH` 时只打一行提示后**忽略**。

## Alternatives considered

- **删掉 `go-gateway/`** —— 它最强的理由是**干净**：不留死代码、不给后来的人
  「这份还能构建吗」的错觉，也省掉「改协议时该改哪份」的判断成本。否掉的原因是
  它仍是一手对照物 —— `go-gateway/internal/zcode/CAPTURED-SPEC.md` 由 Reqable 抓到的
  **成功请求**生成，被仓库约定为「上游协议细节的**唯一可信**规格来源」（见 `AGENTS.md`
  「参考实现的注释是二手信息」）。删掉它等于删掉这份规格。

- **保留 `GOOS`/`GOARCH` 支持** —— 它最强的理由是**保留灵活性**：将来若真要出
  多平台产物，脚本已经支持，不必再改一次；对现有 CI 也没有立即的破坏。否掉的原因是
  那等于维护一条 CI 从不走的路径。取消交叉编译的理由是**故障面**：交叉编译需要为
  目标三元组装 std 与链接器，而 CI 四腿的 host 与 target 本来就一致
  （windows-latest→msvc、macos-14→aarch64-apple-darwin、
  macos-15-intel→x86_64-apple-darwin、ubuntu-22.04→x86_64-unknown-linux-gnu）
  ⇒ 本机产物就是需要的产物，保留交叉只会引入「三元组没装」这一类新故障。

## Consequences

- **收益**：构建链上不再有 Go 依赖 —— 新 clone 的机器与 CI 都不需要 Go 工具链；
  三个 workflow 走的是打包时真正执行的那条路径（`sh scripts/build-gateway.sh`），
  不再存在「脚本能跑但 CI 用另一条命令」的回归空间；交叉编译那条「三元组没装」
  的故障面直接消失。
- **代价与已知上限**：仓库里留着一份不参与构建的 Go 实现，读的人需要知道它
  「不是可构建产物，是一手对照物」；`GOOS`/`GOARCH` 的输入会被**静默忽略**
  （只打一行提示），习惯交叉编译的人可能误以为生效。**什么信号发生时必须重访该决定**：
  - 真要交叉编译时，**不能只设环境变量** —— 必须在 workflow 里显式
    `rustup target add` 并配好链接器，否则得到的是「三元组没装」而不是产物。
  - `go-gateway/` 只有在 `CAPTURED-SPEC.md` 被迁移进 Rust 侧并被同等信任后，
    才可以考虑移出仓库。

## Verification

- 可复核的切断点（三处）：`build-gateway.{sh,ps1}` 内无 `go build` 调用；
  三个 workflow 均调用 `sh scripts/build-gateway.sh`；`ci.yml` / `release.yml` 中
  无 `actions/setup-go`。
- 交叉编译忽略行为：脚本收到 `GOOS`/`GOARCH` 时只输出一行提示，不改变产物。
- 一手规格来源的路径：`go-gateway/internal/zcode/CAPTURED-SPEC.md`（由 Reqable 抓到的
  **成功请求**生成，开头即声明「这是唯一可信的规格来源 —— 此前所有关于该通道的结论
  都是推断，且多次出错」）。
