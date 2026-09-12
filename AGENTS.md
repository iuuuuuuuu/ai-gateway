<!-- TRELLIS:START -->
# Trellis Instructions

These instructions are for AI assistants working in this project.

This project is managed by Trellis. The working knowledge you need lives under `.trellis/`:

- `.trellis/workflow.md` — development phases, when to create tasks, skill routing
- `.trellis/spec/` — package- and layer-scoped coding guidelines (read before writing code in a given layer)
- `.trellis/workspace/` — per-developer journals and session traces
- `.trellis/tasks/` — active and archived tasks (PRDs, research, jsonl context)

If a Trellis command is available on your platform (e.g. `/trellis:finish-work`, `/trellis:continue`), prefer it over manual steps. Not every platform exposes every command.

If you're using Codex or another agent-capable tool, additional project-scoped helpers may live in:
- `.agents/skills/` — reusable Trellis skills
- `.codex/agents/` — optional custom subagents

Managed by Trellis. Edits outside this block are preserved; edits inside may be overwritten by a future `trellis update`.

<!-- TRELLIS:END -->

## UI Component Policy

- For frontend UI, prefer the project's existing shadcn components and compose them before writing custom interactive primitives.
- If a required component is missing, add the matching shadcn/Radix component and wrap it under `src/components/ui/` so styling, accessibility, focus management, and behavior stay consistent.
- Write a custom component only when shadcn components and their composition APIs cannot satisfy the requirement. Record the reason before doing so.
- Custom UI must still reuse the project's Rhea theme tokens, spacing, radii, states, and accessibility conventions. Do not substitute native interactive shortcuts such as `details/summary` when an appropriate shadcn component exists.

## 仓库布局（重要）

网关有两份实现并存，改动前先确认要动的是哪一份：

| 路径 | 角色 |
|---|---|
| `go-gateway/` | 上游 [workbuddy2api](https://github.com/Sliverkiss/workbuddy2api) 的 Go 源码，**已 vendor 进本仓库**（构建期编译并 gzip 内嵌为 `crates/wb-switch-core/embedded/gateway.exe`）。上游改动靠人工同步，不走 submodule。 |
| `crates/wb-switch-gateway/` | 本项目新增的 Rust 版网关（pool / upstream / auth）。**尚未接入主构建流程**，`src-tauri` 仍托管内嵌的 Go 二进制。 |

**不要把 `go-gateway/` 当作只读外部依赖随手改** —— 它同时受上游同步约束；修改需在提交信息里说明是「上游补丁」还是「本项目调整」。

## 构建环境备注（Windows）

- 本机 MSVC toolchain 不可用（`link.exe` 缺失），需使用 `stable-x86_64-pc-windows-gnu`。
- 应用运行时会占用默认 `target/`，跑 `cargo check/test` 建议指定 `CARGO_TARGET_DIR` 绕开。
- `cargo test -p wb-switch-core` 在 Windows 上有 3 个已知失败（路径大小写与分隔符差异），分布在 `codebuddy_cli.rs` / `export_import.rs` / `session.rs`，属预期，非回归。

## Git Commit Language

- Use Conventional Commit type prefixes such as `feat:`, `fix:`, and `docs:`.
- Write the commit subject and body in Chinese by default. Use English only when the user explicitly requests it.
