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
  自动跟版机制。**信号**：若 `agent-notes` 上游脚本接口变动导致本仓库的 npm
  scripts 失效，或有新笔记写进 `docs/DECISIONS.md`（说明规则没被遵守），
  就重访这个决定。
- **代价**：门禁只校验**结构**，不校验**内容**。「Decision 用现在时」「备选方案
  是否真的权衡过」仍是散文纪律，脚本管不到 —— 写得敷衍的笔记照样能过 CI。

## 技能版本与同步（2026-10-18）

项目副本（`.agents/skills/agent-notes/`）**曾落后于插件内置版本**，已同步到
**0.4.1**（内置版 `.synced-version` 的值）。

漂移的具体内容（同步前旧→新）：

| 脚本 | 旧版行为 | 0.4.1 行为 |
|---|---|---|
| `verify-agent-note-tree.ts` | 不支持 **CommonMark 尖括号目标**（用 `&lt;` `&gt;` 包住带空格的目标），把尖括号当路径字符 ⇒ **带空格/特殊字符的目标被误判不存在** | 先剥离尖括号再解析 |
| `verify-agent-note-format.ts` | 不认 `## 后果与边界`、`其他做法` | 两者都算有效别名 |
| `verify-archived-agent-notes.ts` | `AGENTS.md` / `manifest.json` 也进封印清单 | 两者排除（它们不是笔记） |
| `build-board.ts` | 节提取用 `(?=\s*$)`，可能提前截断 | `(?![\\s\\S])` |

★ **同步方向是「内置 → 项目」**：内置版由插件维护并会自动更新，项目副本是
它某一时刻的快照。两者是**两份副本**，项目副本不会被插件自动改（它在 git 跟踪内）。
逆向「从项目推回内置」没有意义 —— 内置版才是上游。

★ **复核方式**（不依赖印象）：对两份目录做逐文件 `Get-FileHash` 比对，
排除 `.synced-version`。本项目当前**零差异**：

```powershell
# 内置目录按你的插件安装位置填；下方是本机实测路径
$src = "<插件内置 skill 目录>"
$dst = ".agents/skills/agent-notes"
Get-ChildItem $src -Recurse -File | Where-Object { $_.Name -ne '.synced-version' } | ForEach-Object {
  $rel = $_.FullName.Replace("$src\",'')
  if ((Get-FileHash $_.FullName).Hash -ne (Get-FileHash (Join-Path $dst $rel)).Hash) { "DIFF $rel" }
}
```

### 顺带接上的看板命令

同步后补了 `npm run notes-board`（对应技能里 `build-board.ts`，**不进 CI**，
与 `check-anchors` 同为本地辅助工具）：

- 产出 `.agents/notes-board.html`（约 72 KB 轻量看板），浏览器打开可浏览全部
  笔记的 Problem / Decision / 备选 / 后果，并支持连接本地目录后热刷新。
- 产物**不入库**：`.gitignore` 的 `.agents/*` 已覆盖它。它是**生成物**，
  每次重建字节都会变，入库只会制造无意义 diff —— 与此前「探针产物不进仓库」
  是同一条纪律。

⚠ **不要用 `Compare-Object (Get-Content a) (Get-Content b)` 判差异** —— 本机
PowerShell 5.1 对 UTF-8 无 BOM 文件的默认读取会产生**假差异**（同一份内容被
读成不同行），据此会得出「项目版更新」的**反向结论**。本次排查就因此一度判断
反了方向，改用 `Get-FileHash` 才对。**字节哈希是唯一可靠判据。**

★ **写笔记时踩到的一件事（门禁在干活）**：本节初稿把「尖括号目标」的**完整字面量**
写进了正文，`verify-agent-note-tree` 立刻红了 —— `LINK_REGEX` 是纯文本匹配
方括号紧跟圆括号的形态，**不认反引号与代码块**，因此任何写进正文的链接样例都会被
当成真链接去解析。修法是把样例包进行内代码并省略可解析的完整形态。
⇒ 结论：**在笔记里举「链接语法」例子时，不要把可解析的字面量写全**；
门禁无法区分「这是例子」与「这是真链接」。本节为此返工**两次**（第二次是描述
正则本身时又写了一个可解析的占位符）。

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

**同步到 0.4.1 后的故障注入**（新能力必须各自证明会红/会绿，不是看代码推断）：

| 注入 | 期望 | 实测 |
|---|---|---|
| 尖括号目标指向不存在的文件（含空格） | tree 红，且报**剥掉尖括号后**的路径 | exit 1，报 `./不 存在 的目标.md` |
| 同上但目标**确实存在**（文件名含空格） | tree 绿 | exit 0 —— 旧版在这里**误报** exit 1 |
| 用 `## 后果与边界` 代替 `## Consequences` | format 绿 | exit 0 |
| 用 `## 其他做法` 代替 `## Alternatives considered` | format 绿 | exit 0 |
| **完全没有**备选节 | format 红（证明别名没把门禁放宽成恒过） | exit 1，`missing ## Alternatives considered` |

（表格里刻意不写出完整的链接字面量 —— 门禁会把它们当成真实链接去解析，
这正是 tree 门禁在干活的表现。）

最后一条是这次特意加的**反向用例**：接受更多别名时最怕顺手把「必写」放宽成
「可有可无」—— 加别名必须配一条「没有它仍然红」的注入。
- CI 侧：`notes` job 在 `ci.yml` 中，`npm ci` 保证 `tsx` 存在；该 job 失败会让整条
  ci 流水线红。
- **结构可复核**：`git ls-files .agents/notes | wc -l` 等于笔记数（当前 23 篇：
  D01–D20 迁移的 20 篇 + 网关启动修复 1 篇 + `Client` 文案修复 1 篇 + 本篇）；
  `git ls-files .agents/skills/agent-notes | wc -l` 为技能文件数（21 个）；
  `git check-ignore -v .agents/skills/trellis-meta/SKILL.md` 命中
  `.gitignore` 的 `.agents/skills/*`。
- 本地已跑 `npm ci`（192 packages）+ `npm run verify-notes`（exit 0）+
  `npm run build`（exit 0），确认新依赖没有破坏既有构建链。
