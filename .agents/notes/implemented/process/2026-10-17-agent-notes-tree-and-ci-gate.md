# Agent Note: 决策笔记树入库，并用 CI 门禁钉住

Status: implemented

## Problem

本仓库的「为什么这么选」此前只有两种载体：`docs/DECISIONS.md`（手写长文，
按主题成篇，2026-10-15 一次成文）和散在 `AGENTS.md` 各节的踩坑更正。两者都能
回答「当初选了什么」，但都回答不了**某个具体改动**的边界：

- 单次改动的取舍（改了哪个行为、否掉了哪条路）没有落点，只能塞进 commit
  message，而 commit message 不进代码树、搜索不到、也不会被后来的人主动打开。
- 没有任何机械校验：格式、必备小节、「备选方案」是否真的写了，全靠自觉。
  写了三篇之后就必然分叉。
- `.agents/` 原先被 `.gitignore` 的 `.agents/` 一条整体排除，等于**任何**写在里面的
  东西都只留在当前这台机器上；换机、换 worktree、CI 全都看不到。

不做的后果是确定的：跨文件的契约变更（例如「前端 ROUTES ↔ 服务端路由」这类
一致性要求）只存在于某次对话里，下一个人改同一处时无从知道前人否掉过什么。

## Decision

采用 `agent-notes` 技能的结构，落在 `.agents/notes/`，并把校验接进 CI。

**目录即状态**：`.agents/notes/{proposed,implemented,rejected,archived}/{class}/yyyy-mm-dd-topic.md`。
`class` 是封闭集：`feature`、`bug-fix`、`simplification`、`architecture`、`process`、
`testing`。空目录不预建 —— 本仓库当前只有 `implemented/process/`。

**入库范围**：`.gitignore` 从 `.agents/` 整体忽略改为 `.agents/*` 加三条放行：

```
!.agents/notes/
!.agents/skills/
.agents/skills/*
!.agents/skills/agent-notes/
```

即**只**把笔记树与 `agent-notes` 这一个技能目录纳入版本控制；`.agents/skills/trellis-*`
（9 个）与 `.codex/`、`.grok/`、`.cursor/`、`.codebuddy/` 仍是本机私货。

**技能随仓库分发**：`agent-notes` 的 `SKILL.md` + `scripts/`（6 个 `.ts`）+
`templates/` + `references/` 全量拷入 `.agents/skills/agent-notes/`（21 个文件，
156 KB）。理由是校验脚本必须在 CI 里跑，而 CI 只能拿到仓库里的文件。

**依赖**：`tsx` 进 `devDependencies`（`^4.23.15`），`package-lock.json` 同批更新。
脚本是 `.ts`，`ci.yml` 的 node 版本是 20，跑不了 Node 原生类型擦除。

**npm scripts**（`package.json`）：

| script | 作用 |
|---|---|
| `verify-agent-note-tree` | 目录/分类/文件名/`INDEX.md` 禁令 + 笔记间相对链接 |
| `verify-agent-note-format` | 头块、状态与 lifecycle 一致、必备小节、备选方案、`implemented` 禁提案标题 |
| `verify-archived` | 归档封印：头布局、`manifest.json` 哈希、只增不改 |
| `verify-notes` | 以上三条串跑（CI 用这个） |
| `archive-agent-note` | 一键归档，`-- <path> [--superseded-by <新笔记>]` |
| `check-anchors` | 软报告，退出码恒 0，**不进 CI** |

**CI**：`ci.yml` 新增 `notes` job（`ubuntu-22.04`，`npm ci` 后跑 `npm run verify-notes`），
不并入三平台 `check` 矩阵 —— 笔记校验与平台无关，跑三遍纯烧时间。
归档封印的基线取变更前的 commit：

```yaml
env:
  AGENT_NOTE_ARCHIVE_BASE_REF: ${{ github.event.pull_request.base.sha || github.event.before }}
```

用 `HEAD` 等于没查（基线就是当前树，永远一致）。新分支 push 时 `before` 是全零，
`git show` 失败后脚本自动跳过只增校验，但**封印与磁盘的哈希比对仍然执行**。

**写笔记的判定**与 DSH 对齐：命中「改了行为 / 架构 / 跨文件契约 / 流程与工具链 /
测试策略 / 落盘·网络·配置格式」即非平凡，必须留笔记；纯格式化、错别字、无歧义
重命名、不改行为的样式与依赖补丁不写。判不准时从严。

## Alternatives considered

- **继续只用 `docs/DECISIONS.md`，不建 `.agents/notes/`** — 它最强的理由是：
  已有 21 条成篇的 D01–D21，读者只需翻一个文件，不存在「同一件事写在两处」的
  风险，也不用新增任何依赖与 CI 步骤。否掉的原因是该文档按**主题**组织、面向
  「这套系统为什么长这样」，而这里要的是按**改动**组织的短记录；把每次小取舍
  都追加进去会让它从 59 KB 膨胀成变更日志，恰恰是它开头明确拒绝的形态
  （「**不是变更日志**」）。两者分工：DECISIONS 记系统级定型，笔记记单次改动的
  边界与否决理由。

- **`npx tsx` 现场下载，不加 devDependency** — 它最强的理由是零新增依赖、
  `package.json` 不动、锁文件不动，技能模板本身就是这么写的。实测在本机
  `npx --yes tsx <file>` 可以跑通。否掉的原因是它把 CI 的可用性绑到 npm registry
  的**每次**拉取上，且版本浮动（今天是 4.x，明年可能是 5.x 而脚本行为变了）——
  一个只做静态检查的门禁不值得引入这种不确定性；`npm ci` 本来就在 CI 里跑，
  加一个 devDependency 的边际成本接近于零。

- **用 Node 原生类型擦除（`node x.ts`）代替 tsx** — 它最强的理由是把依赖数归零，
  而脚本只用 `interface` 与类型注解，不碰 `enum`/`namespace`/参数属性。本机
  Node 25 实测可以直接跑。否掉的原因是 `ci.yml` 钉的是 node 20，原生擦除要
  Node 22.6 起且默认关闭、23 起才默认开启；为了省一个 devDependency 去改 CI 的
  Node 版本，是把验证成本转嫁到所有 job 上。

- **整个 `.agents/` 入库** — 它最强的理由是省掉三条 `.gitignore` 例外，
  谁也不必判断哪个技能该入库。否掉的原因是里面躺着 9 个 `trellis-*` 技能与
  本地导航备忘，属于个人工具链偏好而非项目契约；全量入库会让每次换工具都产生
  无关 diff，也把别人的本机配置带进公开仓库。

- **独立 workflow 文件 `.github/workflows/verify-notes.yml`（技能模板的默认形态）**
  — 它最强的理由是职责单一、删掉笔记体系时直接删一个文件即可，且不影响 `ci.yml`
  的并发组。否掉的原因是本仓库 `ci.yml` 已经承担「push/PR 的机械校验」这一职责，
  再开一个 workflow 会得到两个独立的 `concurrency` 组 —— 同一分支连续 push 时
  两条流水线各自取消，笔记门禁与编译门禁的结论会来自不同 commit。并入 `ci.yml`
  后共享 `concurrency: ci-${{ github.ref }}`。

## Consequences

- **收益**：笔记从「本机私货」变成受版本控制的契约；三条门禁把格式与「备选方案
  必写」变成机械约束，写歪了 CI 直接红，不再依赖自觉。归档一旦封印，改动会被
  `verify-archived` 抓住 —— 「冻结」有了机械含义，而不只是一个形容词。
- **收益**：`.agents/notes/` 与 `docs/DECISIONS.md` 的分工被写明，避免后来者
  把系统级定型也拆成碎片笔记。
- **代价与已知上限**：多一个 `devDependency` 与一条 CI 路径；技能目录（156 KB，
  含 73 KB 的看板 HTML 模板）进了仓库，技能上游更新时需要手工同步 —— 没有
  自动跟版机制，也没有钉住上游 commit 的说明。**信号**：若 `agent-notes` 上游
  脚本接口变动导致本仓库的 npm scripts 失效，或出现「同一决定在笔记与
  `docs/DECISIONS.md` 各写一份且互相矛盾」，就重访这个决定。
- **代价**：门禁只校验**结构**，不校验**内容**。「Decision 用现在时」「备选方案
  是否真的权衡过」仍是散文纪律，脚本管不到 —— 写得敷衍的笔记照样能过 CI。

## Verification

- `npm run verify-notes` 在本机通过（三条脚本各自 `ok:`）；`npm run verify-notes`
  在注入故障时以**退出码 1** 结束（`cmd /c` 取 `%ERRORLEVEL%` 实测，不经管道）。
- **故障注入**（门禁必须先证明会红，才承认它是绿的）：

  | 注入 | 结果 |
  |---|---|
  | `implemented/wrongclass/…md`（未知分类） | `verify-agent-note-tree` → `unknown class folder "wrongclass"`，exit 1 |
  | 缺 `## Alternatives considered` | `verify-agent-note-format` → `missing ## Alternatives considered`，exit 1 |
  | 归档笔记未封印 | `verify-archived` → `missing seal in manifest.json`，exit 1 |
  | 封印后篡改文件 | `verify-archived` → `seal mismatch`，exit 1 |
  | 归档头布局错（缺 `Archived:`） | `verify-archived` → `line 4 must be Archived: YYYY-MM-DD`，exit 1 |
  | 改归档并手工重签 manifest，基线=变更前 commit | `verify-archived` → `seal added/changed/removed relative to <base>`，exit 1 |

  最后一条正是 CI 里 `AGENT_NOTE_ARCHIVE_BASE_REF` 存在的理由：同一份改动把基线
  换成 `HEAD` 时，比较的是「磁盘 vs 磁盘」，只会剩下哈希自校验。所以 workflow 里
  必须传 `pull_request.base.sha || github.event.before`，不能省。
- CI 侧：`notes` job 在 `ci.yml` 中，`npm ci` 保证 `tsx` 存在；该 job 失败会让整条
  ci 流水线红。
- 结构可复核：`git ls-files .agents | wc -l` 为 21（技能 21 个文件）+ 笔记数；
  `git check-ignore -v .agents/skills/trellis-meta/SKILL.md` 命中
  `.gitignore` 的 `.agents/skills/*`。
- 本地已跑 `npm ci`（192 packages）+ `npm run verify-notes`（exit 0）+
  `npm run build`（exit 0），确认新依赖没有破坏既有构建链。
