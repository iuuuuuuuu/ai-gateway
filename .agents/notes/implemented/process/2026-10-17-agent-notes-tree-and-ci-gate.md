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

**`.agents/notes/` 取代 `docs/DECISIONS.md` 成为唯一的决策记录规则，原文档已删除。**
D01–D20 全部**逐条迁移**成 20 篇笔记（`implemented/{architecture,bug-fix,process}/`），
每篇的 `## Problem` 首句标注 `原 `docs/DECISIONS.md` Dxx（从 DECISIONS.md 迁移）`，
以便按编号回溯。`docs/DECISIONS.md` 本体 `git rm` 掉，仓库里不再有两处决策记录。

规则落点：

| 文件 | 改成什么 |
|---|---|
| `AGENTS.md`（根） | 「决策笔记」节声明 `.agents/notes/` 为唯一规则、DECISIONS 已迁入并删除 |
| `docs/AGENTS.md` | 工作流第 1 步与第 4 步的「决策变化」目标改为 `.agents/notes/`；§8 索引表同步 |
| `docs/README.md` | 「怎么找文档」去掉 DECISIONS 行；文档清单删除该条目 |
| `docs/TESTING.md` | 顶部「为什么这么选」的指向改为笔记 |
| `docs/RUNBOOK.md` | 并发上限那条的「需要重新评估」改为指向迁移后的笔记 |
| `docs/FEATURES.md` | 两处「见 DECISIONS.md D18」改为指向笔记树 |
| `README.md`（根） | 文档索引表同步 |
| `docs/DECISIONS.md` | **删除**（内容已全部迁入笔记） |

迁移的 20 篇（按分类）：

| 分类 | 篇数 | 主题 |
|---|---|---|
| `architecture/` | 6 | Go→Rust 重写、Go 参照实现退出构建、网关 gzip 内嵌、数据目录与环境变量、账号库唯一真源、并发上限与换号重试 |
| `bug-fix/` | 9 | 思考档位不预校验、兜底清单不参与声明、Trae 权威例外、熔断同产品兜底、`finish_reason` 幂等、只差 `[DONE]` 不算失败、3012 按（产品,区域）隔离、设备 token 目录发现、11140 误判 |
| `process/` | 5 | 签名密钥不自动搬运、转 private 的自动更新代价、版本积压与产物口径、发版 preflight 门控、glib 告警保持 open |

★ 迁移的纪律：**忠实搬运**，不压缩技术细节、不改结论、不把「未证」补成理由、
不编造原文没有的备选方案。原文「翻案条件」写进每篇的 `## Consequences`
（作为「什么信号发生时必须重访」），原文的实测数据与测试名写进 `## Verification`。

## Alternatives considered

- **继续只用 `docs/DECISIONS.md`，不建 `.agents/notes/`** — 它最强的理由是：
  已有 21 条成篇的 D01–D21，读者只需翻一个文件，不存在「同一件事写在两处」的
  风险，也不用新增任何依赖与 CI 步骤。否掉的原因有两层：一是该文档按**主题**
  组织、面向「这套系统为什么长这样」，而这里要的是按**改动**组织的短记录 ——
  把每次小取舍都追加进去会让它从 59 KB 膨胀成变更日志，恰恰是它开头明确拒绝的
  形态（「**不是变更日志**」）；二是它**没有门禁**，格式与「备选方案必写」全靠
  自觉，而笔记规范带三条机械校验。⇒ 不是两者分工，是**笔记接管**：DECISIONS
  冻结为历史，新增决策一律进 `.agents/notes/`。

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
- **收益**：`.agents/notes/` 成为**唯一**的决策记录规则，`docs/DECISIONS.md` 冻结
  为历史存档。新增决策不再有两个可能的落点，「该写哪份」不再需要判断。
- **代价与已知上限**：多一个 `devDependency` 与一条 CI 路径；技能目录（156 KB，
  含 73 KB 的看板 HTML 模板）进了仓库，技能上游更新时需要手工同步 —— 没有
  自动跟版机制，也没有钉住上游 commit 的说明。**信号**：若 `agent-notes` 上游
  脚本接口变动导致本仓库的 npm scripts 失效，或有新笔记写进 `docs/DECISIONS.md`
  （说明规则没被遵守），就重访这个决定。
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
