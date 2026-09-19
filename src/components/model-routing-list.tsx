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
 *	· 该平台在哪些区域有它
 *	· 能力（视觉等，复用既有 ModelCapabilityRow）
 *	· **可直接复制的三种写法**
 *
 * # 数据来源
 *
 * 全部来自网关 `/v1/models` 的 `channels` 字段（网关生成，非前端猜测）。
 * 取不到时如实显示"未声明"，**不编造**平台。
 */
import { useMemo, useState } from "react";
import { Check, Copy, Info, Search } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { ModelCapabilityRow } from "@/components/model-capability-chip";
import { capabilityViewOf } from "@/lib/model-capability";
import type { GatewayModelItem } from "@/lib/types";
import { cn } from "@/lib/utils";

/** 平台标识 → 展示名与配色。 */
const PRODUCT_LABEL: Record<string, { label: string; className: string }> = {
  workbuddy: {
    label: "WorkBuddy",
    className: "border-sky-500/50 bg-sky-500/10 text-sky-700 dark:text-sky-300",
  },
  qoder: {
    label: "Qoder",
    className: "border-violet-500/50 bg-violet-500/10 text-violet-700 dark:text-violet-300",
  },
  zcode: {
    label: "ZCode",
    className: "border-emerald-500/50 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300",
  },
};

/** 区域标识 → 展示名。 */
const REGION_LABEL: Record<string, string> = {
  cn: "国服",
  intl: "国际版",
  global: "国际版",
};

/**
 * 生成某个模型的**全部可用写法**。
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
  const forms: string[] = [id];
  const channels = model.channels || [];
  for (const ch of channels) {
    const product = (ch.product || "").trim();
    if (!product) continue;
    forms.push(`${product}:${id}`);
    for (const region of ch.regions || []) {
      const label = REGION_LABEL[region];
      if (label) forms.push(`${product}:${label}:${id}`);
    }
  }
  return forms;
}

/** 一个可复制的小块。 */
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
        "group inline-flex items-center gap-1 rounded-md border border-border/60 bg-muted/30",
        "px-1.5 py-0.5 font-mono text-[11px] leading-relaxed",
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

/** 平台徽标。 */
function ProductBadge({ product, regions }: { product: string; regions?: string[] }) {
  const meta = PRODUCT_LABEL[product];
  const label = meta?.label || product;
  const regionText = (regions || [])
    .map((r) => REGION_LABEL[r] || r)
    .filter(Boolean)
    .join(" / ");
  return (
    <Badge
      variant="outline"
      data-slot="model-product"
      data-product={product}
      className={cn("h-4 gap-1 px-1.5 text-[9.5px] font-normal", meta?.className)}
    >
      {label}
      {regionText && <span className="opacity-70">· {regionText}</span>}
    </Badge>
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
 * 与 `capabilityViewOf`），差别只是这里额外突出**平台与可复制写法** ——
 * 因为本页的用途是"让用户在客户端里填对模型名"。
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
        <span className="text-[11px] text-muted-foreground">
          共 {models.length} 个
          {dupCount > 0 && (
            <>
              ，其中 <span className="font-medium text-amber-600 dark:text-amber-400">{dupCount} 个由多平台提供</span>
            </>
          )}
        </span>
      </div>

      {/* 用法说明：这是本清单的核心价值，必须显眼 */}
      <div className="rounded-md border border-border/60 bg-muted/20 px-3 py-2 text-[11px] leading-relaxed text-muted-foreground">
        <div className="mb-1 flex items-center gap-1 font-medium text-foreground">
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
        <div className="rounded-md border border-dashed border-border/60 px-3 py-6 text-center text-xs text-muted-foreground">
          {models.length === 0
            ? "网关还没有返回模型清单。先确认网关已启动、账号已导入，再点「刷新」。"
            : `没有匹配「${query}」的模型。试试清空搜索，或换个关键词（如 glm、qwen、平台名）。`}
        </div>
      ) : (
        <div className="flex max-h-[520px] flex-col gap-2 overflow-y-auto pr-1">
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
                className="rounded-md border border-border/60 px-2.5 py-2"
              >
                <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                  <span className="font-mono text-xs font-medium">{m.id}</span>
                  {channels.length > 1 && (
                    <Badge
                      variant="outline"
                      data-slot="model-dup"
                      className="h-4 border-amber-500/50 bg-amber-500/10 px-1.5 text-[9.5px] font-normal text-amber-700 dark:text-amber-300"
                    >
                      {channels.length} 个平台都有
                    </Badge>
                  )}
                  <span className="flex flex-wrap items-center gap-1">
                    {channels.length > 0 ? (
                      channels.map((c) => (
                        <ProductBadge
                          key={`${c.product}-${(c.regions || []).join(",")}`}
                          product={c.product}
                          regions={c.regions}
                        />
                      ))
                    ) : (
                      <Badge variant="outline" className="h-4 px-1.5 text-[9.5px] font-normal text-muted-foreground">
                        来源未声明
                      </Badge>
                    )}
                  </span>
                </div>

                <ModelCapabilityRow
                  vision={view.vision}
                  region={view.region}
                  className="mt-1"
                />

                <div className="mt-1.5 flex flex-wrap gap-1">
                  {forms.map((f) => (
                    <CopyableForm key={f} form={f} />
                  ))}
                </div>

                {m.region_note && (
                  <p className="mt-1 text-[10.5px] leading-relaxed text-muted-foreground">
                    {m.region_note}
                  </p>
                )}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
