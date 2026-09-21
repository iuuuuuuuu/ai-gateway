<!-- TRELLIS:START -->
# 编码契约（Coding Contracts）

These instructions are for AI assistants working in this project.

本仓库曾由 Trellis 工具初始化。Trellis 的工作流骨架（`workflow.md`、`workspace/`、
`spec/cli/`、`.agents/skills/`、`.codex/agents/`）与 spec 目录均未随仓库分发，
相关引用已于 2026-09-12 清理。

当前生效的编码约定见下方各节（UI Component Policy、Git Commit Language）。

## Shell Policy（强制）

- 本仓库运行环境为 Windows。所有 shell 命令**必须**显式使用 PowerShell（`pwsh` / `powershell`），
  禁止调用 `bash` / `sh` / `zsh` / WSL 及其启动器 `C:\WINDOWS\system32\bash.exe`。
- 调用命令行工具时必须显式指定 shell 为 PowerShell，不要依赖宿主默认值：默认值在
  仅安装了 WSL 启动器、未安装 Linux 发行版的机器上会落到 `bash` 并直接失败。
- 路径一律使用 Windows 形式（`D:\workbuddy2api\...`）、`\` 作为分隔符；环境变量用
  `$env:NAME`。禁止 `curl | sh`、`export VAR=...`、`/dev/null` 等 POSIX 写法。
- 脚本、构建、测试命令一律写成 PowerShell 形式（如 `Get-ChildItem`、`Remove-Item -LiteralPath`、
  `$LASTEXITCODE`）。仓库内已有 `scripts/*.ps1` 的，优先复用而不是重写为 shell 脚本。
- 若某工具确实只提供 POSIX 方式，先确认 `pwsh` 下无等价方案，再在 `AGENTS.md` 记录原因，
  不得静默改用 bash。

Token 统计与网关的接口契约可直接查阅实现本身：

- `crates/ai-gateway-core/src/modules/token_stats.rs` — Token 统计聚合
  （`get_statistics(days: Option<i64>)`）
- `crates/ai-gateway-server/src/api.rs` — HTTP 路由（含 `GET /api/token-stats`）
- `src-tauri/src/commands.rs` — 对应的 Tauri 命令包装

<!-- TRELLIS:END -->

## UI Component Policy

- For frontend UI, prefer the project's existing shadcn components and compose them before writing custom interactive primitives.
- If a required component is missing, add the matching shadcn/Radix component and wrap it under `src/components/ui/` so styling, accessibility, focus management, and behavior stay consistent.
- Write a custom component only when shadcn components and their composition APIs cannot satisfy the requirement. Record the reason before doing so.
- Custom UI must still reuse the project's Rhea theme tokens, spacing, radii, states, and accessibility conventions. Do not substitute native interactive shortcuts such as `details/summary` when an appropriate shadcn component exists.

## 模型列表（Model List Policy，强制）

**严禁在任何位置内置静态模型表 / 静态模型清单**（Rust 常量、`static` / `const` 数组、
前端 `MODEL_*` 常量、JSON 内嵌兜底表、TS 里的 `DEFAULT_MODELS` 等一律禁止）。

- 模型的唯一来源是**运行中的网关**：`GET /v1/models`。网关自己再向上游现拉
  （见 `crates/ai-gateway-router/src/upstream/client.rs` 的 `fetch_models`，
  数据取自 `data.agents[name=="cli"].models`），要求**实时**获取。
- 拉取入口统一走已有的实现，不要另写一份：
  - Rust：`crates/ai-gateway-core/src/modules/gateway.rs` 的 `pub async fn fetch_models()`
  - HTTP 路由：`crates/ai-gateway-server/src/api.rs`
  - Tauri 包装：`src-tauri/src/commands.rs`
  - 前端：`src/lib/api.ts` 的 `getGatewayModels()`，视图见
    `src/components/agents/models-view.tsx`、`src/pages/AgentsPage.tsx`
- **拉不到就明说拉不到**：返回空列表 + 具体错误原因（例如
  `网关未运行：模型清单来自上游，需先启动网关再刷新`），由 UI 明确呈现给用户。
  严禁用静态表兜底、严禁伪造一份清单、严禁「拉取失败但界面照常有模型可选」。
- 唯一允许的"兜底"是空：`Vec::new()` / `[]` + 错误提示。任何形式的
  `if 拉取失败 { 用内置常量 }` 都属于违规。
- 新增客户端接入 / 模型分发相关代码时同样遵守：模型候选只能来自这次实时拉取的结果，
  不得在配置文件模板、默认值、示例里写死具体模型 ID 作为可用清单。

原因：静态表会与上游真实清单形成两份各自失真的副本，模型下线或改名后界面仍显示旧条目，
用户按界面选了不可用的模型，问题被静默掩盖。

若确有必要引入任何形式的模型兜底数据，必须先在 `AGENTS.md` 记录原因与范围，
不得直接写进代码。

## 构建与签名（Build & Signing）

构建带 updater 签名的安装包时，**不要再去搜索私钥**，位置与用法如下（固定不变）：

- 一条命令：`pwsh scripts/build-signed.ps1`（仅校验密钥不构建：加 `-CheckOnly`）
- 签名私钥：`%USERPROFILE%\.ai-gateway\ai-gateway-updater.key`（minisign 私钥）
- 私钥口令：`%USERPROFILE%\.ai-gateway\ai-gateway-updater.password`
- 两者都在**仓库外**，`.gitignore` 已排除 `*.key`；**本仓库是公开仓库，严禁把口令写入任何被 git 跟踪的文件。**

背景（改动相关代码前务必了解，否则会重复踩坑）：

- `src-tauri/tauri.conf.json` 的 `createUpdaterArtifacts` **一直为 `true`**，因此
  `tauri build` 必须拿到私钥；`tauri.conf.json` 里的 `plugins.updater.pubkey`
  必须与私钥配对（keyid `217C0E2B4321D841`），否则客户端会拒绝更新包。
- `TAURI_SIGNING_PRIVATE_KEY` 的值必须是密钥**内容**，不是路径。传路径会报
  `failed to decode base64 secret key: Invalid symbol 58`（路径里的冒号）。
  读取方式：`(Get-Content -LiteralPath $key -Raw).Trim()`。
- **缺口令时 `tauri signer sign` 不报错，而是阻塞等待 stdin 输入**，表现为"构建卡住"；
  口令错误才会秒级报错。所以非交互场景务必先确认口令可用（`-CheckOnly` 就是为此）。
- 加密私钥的 keynum 偏移（54..62）与公钥（2..10）不同，**不能直接比对**；
  判断配对是否正确的唯一可靠方式是实际签一次，再比签名与公钥的 keyid。

## Git Commit Language

- Use Conventional Commit type prefixes such as `feat:`, `fix:`, and `docs:`.
- Write the commit subject and body in Chinese by default. Use English only when the user explicitly requests it.
