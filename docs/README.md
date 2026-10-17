# 文档地图

> 本页说明 `docs/` 里每份文档「为什么存在、什么时候该翻它」。**改文档结构时同步更新本页。**
> 最后核对：2026-10-15 ｜ 对应分支 `main` ｜ 版本 **1.0.46** ｜ 核对时 HEAD `dc729e9`

## 怎么找文档

| 我想…… | 翻这份 |
|---|---|
| **开始动手改代码** | [AGENTS.md](AGENTS.md) —— 开发入口，开工第一件事 |
| 知道「为什么当初这么选」/ 想推翻旧结论 | **`.agents/notes/`** —— 决策笔记，唯一规则（`{lifecycle}/{class}/yyyy-mm-dd-topic.md`） |
| 发版、在本机跑起来、按现象排障、回滚 | [RUNBOOK.md](RUNBOOK.md) |
| 写测试 / 跑验收 / 怀疑门禁是不是假的 | [TESTING.md](TESTING.md) |
| 改用户能看见的行为之前对齐语义 | [FEATURES.md](FEATURES.md) |
| 看用户视角的功能导览与安装说明 | [../README.md](../README.md) |
| 查硬约束（Shell / 组件 / 构建签名 / 验证纪律） | [../AGENTS.md](../AGENTS.md) —— 仓库根，与 [AGENTS.md](AGENTS.md) 并存 |
| 纯部署版（server 单文件）怎么发 | [SERVER-DEPLOY-1.0.46.md](SERVER-DEPLOY-1.0.46.md) |
| 查「实现搬过来了、接线丢了」这类 P0 | [GO-STARTUP-WIRING-AUDIT.md](GO-STARTUP-WIRING-AUDIT.md) |

## 文档清单

| 文档 | 定位 | 更新触发 |
|---|---|---|
| [AGENTS.md](AGENTS.md) | 开发入口：环境与命令、硬约束、工作流、验证纪律 | 命令/约束/流程变化时 |
| [RUNBOOK.md](RUNBOOK.md) | 发版、签名、本机运行、按现象排障、回滚 | 发布流程或排障手法变化时 |
| [TESTING.md](TESTING.md) | 门禁与基线、四层测试、真机验收约定、禁止假绿门禁 | 测试体系或基线变化时 |
| [FEATURES.md](FEATURES.md) | 用户可见行为规格（路由、报错文案、流式收尾、用量护栏） | 行为变化时 |
| [SERVER-DEPLOY-1.0.46.md](SERVER-DEPLOY-1.0.46.md) | 1.0.46 纯部署服务端的交付物、校验与部署步骤 | 该产物重新打包时 |
| [GO-STARTUP-WIRING-AUDIT.md](GO-STARTUP-WIRING-AUDIT.md) | Go→Rust 迁移期的启动接线穷尽对照（**历史审计，只读**） | 不再更新；结论过时在新笔记里翻案 |

> 仓库根另有两份**长期存在**的文档，不属于 `docs/` 但同样重要：
> `README.md`（用户视角导览）、`AGENTS.md`（编码契约硬约束）。

## 写文档的约定

- **只写代码里读不出来的**：为什么这样做、约束、陷阱、验收标准、非显然步骤。**不复述代码结构** —— 结构会变，读代码比读文档快。
- **每条结论带日期 + 适用版本或 commit 范围**；**过期即删**，不留「历史遗留」章节。
- 文档与代码**同一次提交**更新；改文档结构时同步本页索引。
- 判据：**别人 clone 到就成立** → 进仓库文档；**只对本机/本人成立**（本机路径、探针产物、真实密钥、真实账号标识）→ 不进仓库。
- 命令一律写成 **PowerShell** 形式（本仓库强制，见根 `AGENTS.md` 的 Shell Policy）。
- 中文文档，**UTF-8 无 BOM 问题**（`.ps1` 例外，必须带 BOM，见根 `AGENTS.md`）。
