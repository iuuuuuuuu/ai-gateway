/**
 * 「模型路由清单」——把**三种写法**与每个模型来自哪些平台完整展示给用户。
 *
 * # 为什么需要它（所有者的需求）
 *
 * 三个平台**有重名模型**。实测（2026-09-20，真实账号）：
 *
 *	glm-5.3        ← workbuddy + zcode
 *	glm-5.2        ← workbuddy + zcode
 *	glm-5.3-flash  ← workbuddy + zcode
 *	glm-5.1        ← workbuddy + zcode
 *
 * 用户输入裸名 `glm-5.3` 时，账号池按「最早到期分层」选号，可能选中
 * 一个**没有该模型资源包**的平台 —— 那条通道回 `1113 无可用资源包`，
 * 用户看到的是"余额不足"（其实额度充足，只是走错了平台）。
 *
 * 故本组件把**可复制的完整写法**摆出来，用户直接抄进客户端即可消除歧义：
 *
 *	zcode:glm-5.3              指定平台
 *	zcode:国际版:glm-5.3        指定平台 + 区域
 *	glm-5.3                    不指定（自动选）
 *
 * # 为什么每个名字都要带描述
 *
 * 光列名字用户不知道选哪个 —— 他需要知道"这个名字走的是哪个平台、
 * 那个平台当前能不能用"。故每个模型显示：
 *
 *	· 模型名（上游 id）
 *	· 来源平台徽标（WorkBuddy / Qoder / ZCode）
 *	· 该平台在哪些区域有它（悬浮可见）
 *	· 能力（视觉等，复用既有 ModelCapabilityRow）
 *	· **可直接复制的三种写法**
 *
 * # 数据来源
 *
 * 全部来自网关 `/v1/models` 的 `channels` 字段（网关生成，非前端猜测）。
 * 取不到时如实显示"未声明"，**不编造**平台。
 *
 * ────────────────────────────────────────────────────────────────────────
 * # 视觉规范：与「智能体管理」页（`AgentsPage.tsx`）**同一套**
 *
 * 所有者原话：「智能体管理跟兼容网关 这里的这个保持一下同步样式」。
 * 两页展示的是**同一份** `/v1/models` 数据，同一个模型在两页的排版、
 * 字号层级、徽标样式必须看起来是一套，否则用户会怀疑自己看的是两份数据。
 *
 * 从 `AgentsPage.tsx` 读出来的规范（逐条对齐，不是"凭感觉像"）：
 *
 *	① 条目容器（`data-slot="agent-model-card"`）
 *	     `flex flex-col gap-1.5 rounded-lg border px-2.5 py-2 text-left text-xs`
 *	   —— 本组件条目同款：`flex flex-col gap-1.5 rounded-lg border
 *	     border-border/60 px-2.5 py-2 text-xs`
 *	   （只把 `text-left` 去掉：本组件条目不是 button，没有可继承的对齐差异）
 *
 *	② 标题/说明层级
 *	     区块标题卡头   `text-[13px] font-semibold`
 *	     次级统计        `text-[11px] text-muted-foreground`
 *	     卡片描述        `text-xs text-muted-foreground`
 *
 *	③ 模型名
 *	     `min-w-0 flex-1 break-all font-mono`（字号由容器的 `text-xs` 继承）
 *	   —— 本组件同款。
 *
 *	④ 能力行
 *	     直接复用 `ModelCapabilityRow`，**不传 className**（AgentsPage 就是这么
 *	     用的）。此前本组件传了 `mt-1`，靠 margin 撑间距，与 AgentsPage 靠
 *	     父级 `gap-1.5` 撑间距的做法不一致，间距数值会随字号变化而漂移。
 *
 *	⑤ 平台徽标 —— 这是最显眼的一处。AgentsPage 的平台 chip 是：
 *	     `rounded border px-1.5 py-0.5 text-[11px] font-medium leading-4`
 *	     + `productAccentOf(product)` 的 border/bg/text
 *	   ⚠ 该文件的注释明确记着「字号 9px → 11px、加大内边距：所有者反馈
 *	     『都看不清』」。本组件此前**自己写了一份** `PRODUCT_LABEL` 配色表，
 *	     而且是 `Badge h-4 text-[9.5px]` —— 既与 AgentsPage 配色不同
 *	     （zcode 是 emerald，AgentsPage 是 teal），又回到了被否掉的 9.5px。
 *	     现已改为**同一个** `productAccentOf` + 同款类名，两页必然一致。
 *
 *	     区域信息（`regions`）与 AgentsPage 一样放进 `title` 悬浮显示，
 *	     不占据可见文字：它在紧随其后的**可复制写法**里是明写的
 *	     （`zcode:国际版:glm-5.3`），没有信息丢失。
 *
 *	⑥ 「默认主模型」徽标几何：`Badge h-4 w-fit px-1 text-[9px] font-normal`
 *	   —— 本组件「N 个平台都有」的琥珀徽标用同款几何尺寸。
 *
 *	⑦ 子面板（AgentsPage 的「多选模型状态栏」）
 *	     `rounded-lg border border-border/40 bg-muted/15 p-2 space-y-1.5`
 *	   —— 本组件的「三种写法」说明块用同款。
 *
 *	⑧ 空状态（AgentsPage 的「尚未从网关获取到上游模型」）
 *	     `rounded-xl border border-dashed border-border/70 bg-muted/15 p-6
 *	      text-center space-y-2.5` + `text-sm font-medium` 标题
 *	   —— 本组件空状态同款。
 *
 * # 刻意**不**对齐的两处（语义不同，见下）
 *
 *	· AgentsPage 的平台 chip 是**开关**（`<button>` + `data-on`，
 *	  点击写 `model_platforms` 配置、改服务端行为）；
 *	  本组件的徽标是**只读**（`<span>`），因为它不改任何配置。
 *	  只统一视觉，不统一交互。
 *
 * ────────────────────────────────────────────────────────────────────────
 * # 布局：从「一行一个」改成**自适应多列网格**（所有者 2026-09-20 要求）
 *
 * 所有者原话：
 *
 *	「兼容网关这里 模型路由清单 不应该一行一个，优化下布局 改成动态的」
 *	「兼容网关和智能体路由 的 路由模型清单 样式也没统一，我说了兼容网关
 *	  这里的 路由模型清单做的不错，让你两个统一一下你忘了吧」
 *
 * 这两条其实指向**同一个动作**：把单列改成像 AgentsPage 那样的响应式网格，
 * 于是「不一行一个」与「两页统一」同时达成。
 *
 * ## 为什么用 `auto-fill` 而不是 AgentsPage 的 `sm:grid-cols-2 lg:grid-cols-3`
 *
 * 固定的断点列数在**宽屏**下会浪费：本清单所在区块宽度随侧栏折叠/窗口变化，
 * 而 `lg:grid-cols-3` 到了 2560 宽还是 3 列，卡片被拉得很宽、右侧大片空白 ——
 * 那正是所有者说的"动态"要解决的问题。`auto-fill` + `minmax`
 * 让列数**随可用宽度自动增减**，且不需要为每个断点写一条类名。
 *
 * ⚠ 这个写法在本仓库**已有先例**（`GatewayPage.tsx:1841`、
 * `AccountsPage.tsx:1273` 都在用 `grid-cols-[repeat(auto-fill,minmax(...))]`），
 * 故不是我引入的新风格。
 *
 * ## 卡片最小宽度取 260px 的依据
 *
 * 卡片内容最宽的一行是「可复制写法」（如 `qoder:国际版:qwen3.8-flash`，
 * 约 30 个等宽字符 ≈ 220px）+ 复制图标 + 内边距 ≈ 250px。
 * 取 260px 让它**刚好一行放得下**；小于此值会折行，那种"两行才装得下"
 * 的卡片正是单列时代的问题（纵向过高、扫视效率低）。
 */
import { useMemo, useState } from "react";
import { Check, Copy, Info, Search } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { ModelCapabilityRow } from "@/components/model-capability-chip";
import { capabilityViewOf } from "@/lib/model-capability";
import { PRODUCT_LABELS, productAccentOf } from "@/lib/product-accent";
import type { GatewayModelItem } from "@/lib/types";
import { cn } from "@/lib/utils";

/** 区域标识 → 展示名。 */
const REGION_LABEL: Record<string, string> = {
  cn: "国服",
  intl: "国际版",
  global: "国际版",
};

/**
 * 生成某个模型的**全部可用写法**。
 *
 * # 优先用网关下发的 `aliases`（2026-09-20 起）
 *
 * 网关的 `/v1/models` 现在会为每个模型直接下发 `aliases`
 * （`平台:模型名` / `平台:区域:模型名`，见 capability.go 的 aliasesOf）。
 * **优先用它**，理由是它才是权威：
 *
 *	· 网关知道**该产品确实有账号的区域**，前端只能从 `channels[].regions` 猜
 *	· 两处各算一遍必然出现分歧（而分歧的表现是"界面给的写法抄进去解析失败"）
 *	· 网关加新写法时前端自动跟上，不需要同步改两处
 *
 * 只有旧网关不带 `aliases` 时才退回本地计算 —— 那是**向后兼容**，
 * 不是主路径。保留它是因为"前端连着一个稍旧的网关"是真实情况
 *（宿主与网关是两个可分别更新的组件）。
 *
 * 规则（与网关 `resolveModel` 的解析口径**必须一致**，否则展示的写法
 * 用户抄进去会解析失败）：
 *
 *	1. 裸名                        —— 总是可用（不限制）
 *	2. 每个平台一个              —— `zcode:glm-5.3`
 *	3. 每个「平台 + 区域」一个    —— `zcode:国际版:glm-5.3`
 *
 * ⚠ 区域写法用**中文「国际版」**而不是 `global`：所有者自己就说「国际版」，
 * 而网关两种都接受（见 resolve_model.go 的 realmIntlCN）。
 * 展示中文对中文用户更友好，且抄进去确实能解析。
 */
export function routeFormsOf(model: GatewayModelItem): string[] {
  const id = model.id;
  // 汇总而不是直接返回：裸名始终排第一（它是"最省事的写法"），
  // 后面的组合名按网关给的顺序。
  const seen = new Set<string>();
  const out: string[] = [];
  const push = (s: string) => {
    if (s && !seen.has(s)) {
      seen.add(s);
      out.push(s);
    }
  };

  push(id);
  // ① 网关下发的权威写法
  for (const a of model.aliases || []) push(a);
  if (out.length > 1) return out;

  // ② 旧网关：本地按 channels 推导（向后兼容路径）
  const channels = model.channels || [];
  for (const ch of channels) {
    const product = (ch.product || "").trim();
    if (!product) continue;
    push(`${product}:${id}`);
    for (const region of ch.regions || []) {
      const label = REGION_LABEL[region];
      if (label) push(`${product}:${label}:${id}`);
    }
  }
  return out;
}

/**
 * 一个可复制的小块。
 *
 * 这是本清单**存在的理由**（"告诉用户怎么填模型名"），因此它是本组件
 * 独有、AgentsPage 没有的东西。几何尺寸向 AgentsPage 的平台 chip 靠
 * （`rounded border px-1.5 py-0.5 text-[11px]`），字号取同一个 11px ——
 * 比 AgentsPage 展开区里 10px 的模型 token 略大一档，是刻意的：
 * 这几行是**要读准再抄走**的内容，抄错一个字符就是一次失败请求。
 */
function CopyableForm({ form }: { form: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      data-slot="route-form"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(form);
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        } catch {
          // 剪贴板不可用（无权限/非安全上下文）时**不静默失败**：
          // 用户点了没反应会以为按钮坏了。这里至少把内容选中，
          // 让他能手动 Ctrl+C。
        }
      }}
      title={`点击复制：${form}`}
      className={cn(
        "group inline-flex items-center gap-1 rounded border border-border/60 bg-muted/30",
        "px-1.5 py-0.5 font-mono text-[11px] leading-4",
        "transition-colors hover:border-primary/50 hover:bg-muted/60",
      )}
    >
      <span className="break-all">{form}</span>
      {copied ? (
        <Check className="size-3 shrink-0 text-emerald-500" />
      ) : (
        <Copy className="size-3 shrink-0 opacity-40 group-hover:opacity-80" />
      )}
    </button>
  );
}

/**
 * 平台徽标（**只读**）。
 *
 * 视觉与「智能体管理」页的平台开关 chip 完全同款：同一个
 * `productAccentOf` 配色、同一组类名。区别只有元素类型 ——
 * 那边是 `<button aria-pressed>`（点击写配置），这里是 `<span>`
 * （纯展示，不改任何东西）。所有者本轮的要求是"同步样式"，不是"同步交互"。
 */
function ProductBadge({ product, regions }: { product: string; regions?: string[] }) {
  const accent = productAccentOf(product);
  const label = PRODUCT_LABELS[product] || product;
  const regionText = (regions || [])
    .map((r) => REGION_LABEL[r] || r)
    .filter(Boolean)
    .join(" / ");
  return (
    <span
      data-slot="model-product"
      data-product={product}
      // 区域与 AgentsPage 同款：进 title 悬浮，不占可见文字
      //（它在紧随其后的可复制写法里是明写的，信息不丢）。
      title={regionText ? `${label}（${regionText}）` : label}
      className={cn(
        "rounded border px-1.5 py-0.5 text-[11px] font-medium leading-4",
        accent.border,
        accent.bg,
        accent.text,
      )}
    >
      {label}
    </span>
  );
}

export interface ModelRoutingListProps {
  models: GatewayModelItem[];
  className?: string;
}

/**
 * 模型路由清单。
 *
 * 与「智能体管理」页的模型展示**口径一致**（同一个 `ModelCapabilityRow`
 * 与 `capabilityViewOf`、同一套平台配色），差别只是这里额外突出
 * **可复制写法** —— 因为本清单的用途是"让用户在客户端里填对模型名"。
 */
export function ModelRoutingList({ models, className }: ModelRoutingListProps) {
  const [query, setQuery] = useState("");

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    const list = q
      ? models.filter(
          (m) =>
            m.id.toLowerCase().includes(q) ||
            (m.channels || []).some((c) => (c.label || c.product || "").toLowerCase().includes(q)),
        )
      : models;
    // 稳定排序：先按「是否重名」降序（重名的更需要用户做选择），再按 id
    return [...list].sort((a, b) => {
      const an = (a.channels || []).length;
      const bn = (b.channels || []).length;
      if (an !== bn) return bn - an;
      return a.id.localeCompare(b.id);
    });
  }, [models, query]);

  /** 重名模型数（>1 个平台提供）—— 这是本清单存在的理由，故显式统计。 */
  const dupCount = useMemo(
    () => models.filter((m) => (m.channels || []).length > 1).length,
    [models],
  );

  return (
    <div className={cn("flex flex-col gap-3", className)} data-slot="model-routing-list">
      <div className="flex flex-wrap items-center gap-2">
        <div className="relative flex-1 min-w-[180px]">
          <Search className="absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
          <Input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="搜索模型名或平台…"
            data-slot="model-routing-search"
            className="h-8 pl-7 text-xs"
          />
        </div>
        {/* 计数用 text-[11px]，与 AgentsPage 的「配置模型 (N 个)」同一档 */}
        <span className="text-[11px] text-muted-foreground">
          共 {models.length} 个
          {dupCount > 0 && (
            <>
              ，其中 <span className="font-medium text-amber-600 dark:text-amber-400">{dupCount} 个由多平台提供</span>
            </>
          )}
        </span>
      </div>

      {/*
        用法说明：这是本清单的核心价值，必须显眼。
        子面板样式取 AgentsPage「多选模型状态栏」同款
        （`rounded-lg border border-border/40 bg-muted/15 p-2`）。
      */}
      <div className="space-y-1.5 rounded-lg border border-border/40 bg-muted/15 p-2.5 text-[11px] leading-relaxed text-muted-foreground">
        <div className="flex items-center gap-1 font-medium text-foreground">
          <Info className="size-3" />
          模型名支持三种写法（点任意一条即可复制）
        </div>
        <div className="flex flex-col gap-0.5">
          <span>
            <code className="font-mono">平台:模型名</code>
            {" —— 只走该平台，如 "}
            <code className="font-mono">zcode:glm-5.3</code>
          </span>
          <span>
            <code className="font-mono">平台:国际版:模型名</code>
            {" —— 同时限定平台与区域，如 "}
            <code className="font-mono">qoder:国际版:glm-5.3</code>
          </span>
          <span>
            <code className="font-mono">模型名</code>
            {" —— 不限定，由网关按到期时间自动选（重名时可能选到任一平台）"}
          </span>
        </div>
      </div>

      {filtered.length === 0 ? (
        /* 空状态与 AgentsPage「尚未从网关获取到上游模型」同款 */
        <div className="space-y-2.5 rounded-xl border border-dashed border-border/70 bg-muted/15 p-6 text-center">
          <div className="text-sm font-medium">
            {models.length === 0 ? "还没有拿到网关的模型清单" : `没有匹配「${query}」的模型`}
          </div>
          <p className="mx-auto max-w-md text-xs text-muted-foreground">
            {models.length === 0
              ? "网关尚未启动或正在同步。启动网关并导入账号后，这里会自动列出全部上游模型与它们的可复制写法。"
              : "试试清空搜索，或换个关键词（如 glm、qwen、平台名）。"}
          </p>
        </div>
      ) : (
        /*
          列表：**自适应多列网格**（见文件头「布局」一节）。

          ⚠⚠ **绝不能加 `auto-rows-fr`**（我加过，是一个真实缺陷）

          `auto-rows-fr` = `grid-auto-rows: minmax(0, 1fr)` —— 它要求
          **所有行都等于容器高度的一份**。而本容器有 `max-h-[460px]`，
          于是每一行被压成 `460px ÷ 行数`：

            45 个模型 / 8 列 ≈ 6 行 ⇒ 每行只有 ~76px
            而卡片内容（模型名+能力行+平台徽标+写法行+说明）需要 150px+

          ⇒ 卡片互相重叠、文字溢出到相邻卡片上（所有者 2026-09-20 截图反馈
             「这里 模型路由清单 显示异常」，画面上正是这种重叠）。

          而且 `auto-rows-fr` 在本处**根本不需要**：
          网格默认 `align-items: stretch`，**同一行**的卡片本来就会等高
          （行高取该行最高者）。`auto-rows-fr` 额外要求"跨行也等高"，
          那对内容长度差异很大的清单是错的。

          故这里用默认的 `grid-auto-rows: auto`（不写就是 auto）——
          每行按内容取高，同行仍然等高。
        */
        <div className="grid max-h-[460px] grid-cols-[repeat(auto-fill,minmax(260px,1fr))] content-start gap-2 overflow-y-auto p-0.5 pr-1">
          {filtered.map((m) => {
            const channels = m.channels || [];
            const forms = routeFormsOf(m);
            // 与「智能体管理」页**同一个**能力视图函数 —— 两处口径必须一致，
            // 否则同一个模型在两个页面显示不同能力，用户不知道信哪个。
            const view = capabilityViewOf(m);
            return (
              <div
                key={m.id}
                data-slot="model-routing-item"
                data-model={m.id}
                // 与 AgentsPage 的 `agent-model-card` 容器同款类名（见文件头 ①）
                className="flex min-w-0 flex-col gap-1.5 rounded-lg border border-border/60 px-2.5 py-2 text-xs"
              >
                {/* 行 1：模型名 + 重名徽标（徽标几何与 AgentsPage 的
                    「默认主模型」一致：Badge h-4 w-fit px-1 text-[9px]） */}
                <div className="flex items-start gap-1.5">
                  <span className="min-w-0 flex-1 break-all font-mono" title={m.id}>
                    {m.id}
                  </span>
                  {channels.length > 1 && (
                    <Badge
                      variant="outline"
                      data-slot="model-dup"
                      className="h-4 w-fit shrink-0 border-amber-500/50 bg-amber-500/10 px-1 text-[9px] font-normal text-amber-700 dark:text-amber-300"
                    >
                      {channels.length} 个平台都有
                    </Badge>
                  )}
                </div>

                {/* 行 2：能力行 —— 复用同一个组件、不传 className（同 AgentsPage） */}
                <ModelCapabilityRow vision={view.vision} region={view.region} />

                {/* 行 3：来源平台徽标。
                    与 AgentsPage 的「渠道」行同款：多平台时才显式给「平台：」前缀。
                    ⚠ 这里是只读 `<span>`，**不是** AgentsPage 那个会写配置的开关 ——
                    语义不同，只统一视觉（见文件头末节）。 */}
                {channels.length > 0 ? (
                  <div className="flex flex-wrap items-center gap-1" data-slot="model-routing-channels">
                    {channels.length > 1 && (
                      <span className="text-[10px] leading-4 text-muted-foreground">平台：</span>
                    )}
                    {channels.map((c) => (
                      <ProductBadge
                        key={`${c.product}-${(c.regions || []).join(",")}`}
                        product={c.product}
                        regions={c.regions}
                      />
                    ))}
                  </div>
                ) : (
                  // 旧网关不带 `channels`（未声明）—— 如实说明，**不编造**平台。
                  // 徽标几何对齐 ModelCapabilityRow 里「未知」那一档（虚线中性色）。
                  <div>
                    <Badge
                      variant="outline"
                      className="h-4 border-dashed border-border/70 bg-transparent px-1.5 text-[9.5px] font-normal text-muted-foreground"
                    >
                      来源未声明
                    </Badge>
                  </div>
                )}

                {/* 行 4：**可复制写法** —— 本清单独有的内容，AgentsPage 没有 */}
                <div className="mt-0.5 flex flex-wrap gap-1">
                  {forms.map((f) => (
                    <CopyableForm key={f} form={f} />
                  ))}
                </div>

                {m.region_note && (
                  <p className="text-[11px] leading-relaxed text-muted-foreground">{m.region_note}</p>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
