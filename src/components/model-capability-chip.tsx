import { CircleHelp, Eye, EyeOff, Gauge, Globe, Ruler } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from "@/components/ui/tooltip";
import type { CapabilityDisplay, RegionHint } from "@/lib/model-capability";
import { cn } from "@/lib/utils";

/**
 * model-capability-chip.tsx 展示模型的三项能力：上下文 / 视觉 / 思考档位。
 *
 * 复用而非新建 shadcn 组件：本文件只用已有的 `Badge` 与 `Tooltip`（含
 * `TooltipProvider`/`Trigger`/`Content`）组合，没有任何自创交互原语 ——
 * 未知态的说明走 Tooltip（Radix 提供焦点管理与 `aria-describedby`），
 * 不用原生 `title` 之外的 `details/summary` 之类快捷键。
 *
 * 为什么「未知」要有**独立的一档颜色与图标**（而不是和「不支持」共用）：
 * 本项目的语义是「—」= 不知道、`false`/空 = 确定的否定。两者视觉上若一致，
 * 用户会把「网关没拉到真值」读成「这个模型不支持图片」，进而关掉一个
 * 本来可用的能力（capability.go 的注释明确警告过这个方向的错误）。
 */

/** 单项能力的紧凑徽标。 */
function CapabilityChip({
  icon: Icon,
  display,
  dataSlot,
  className,
}: {
  icon: React.ComponentType<{ className?: string }>;
  display: CapabilityDisplay;
  dataSlot: string;
  className?: string;
}) {
  const body = (
    <span
      data-slot={dataSlot}
      data-unknown={display.unknown ? "true" : "false"}
      className={cn(
        "inline-flex items-center gap-1 rounded border px-1.5 py-0.5 text-[10px] font-medium",
        display.unknown
          ? // 未知：中性虚线 —— 视觉上就与「确定的否」不同
            "border-dashed border-border/70 bg-transparent text-muted-foreground"
          : "border-border/60 bg-muted/30 text-foreground",
        className,
      )}
    >
      <Icon className="size-2.5 shrink-0" />
      {display.text}
    </span>
  );

  if (!display.title) return body;
  return (
    <TooltipProvider>
      <Tooltip>
        <TooltipTrigger asChild>{body}</TooltipTrigger>
        <TooltipContent className="max-w-xs text-[11px] leading-relaxed">{display.title}</TooltipContent>
      </Tooltip>
    </TooltipProvider>
  );
}

export interface ModelCapabilityRowProps {
  context: CapabilityDisplay;
  vision: CapabilityDisplay;
  efforts: CapabilityDisplay;
  region?: RegionHint;
  className?: string;
}

/**
 * 一个模型的「上下文 · 视觉 · 思考档位」能力行。
 *
 * 档位**逐档列出**（所有者要求「信息不能为了清爽而丢」）：只给「支持 3 档」
 * 会把他真正要选的 `max` 藏起来，那正是这次需求要解决的问题。
 */
export function ModelCapabilityRow({
  context,
  vision,
  efforts,
  region,
  className,
}: ModelCapabilityRowProps) {
  // 思考档位可能很长（实测 gpt-6-astra 有 5 档：low/medium/high/xhigh/max），
  // 因此它单独占一行、允许折行；上下文与视觉属于短标量，与区域徽标同排。
  return (
    <div className={cn("flex flex-col gap-1", className)} data-slot="model-capability-row">
      <div className="flex flex-wrap items-center gap-1">
        <CapabilityChip
          icon={Ruler}
          display={context}
          dataSlot="model-capability-context"
        />
        <CapabilityChip
          icon={vision.unknown ? CircleHelp : vision.text === "视觉" ? Eye : EyeOff}
          display={vision}
          dataSlot="model-capability-vision"
        />
        {region && (
          <TooltipProvider>
            <Tooltip>
              <TooltipTrigger asChild>
                <Badge
                  variant="outline"
                  data-slot="model-capability-region"
                  className="h-4 gap-1 border-amber-500/50 bg-amber-500/10 px-1.5 text-[9.5px] font-normal text-amber-700 dark:text-amber-300"
                >
                  <Globe className="size-2.5" />
                  {region.label}
                </Badge>
              </TooltipTrigger>
              <TooltipContent className="max-w-xs text-[11px] leading-relaxed">
                {region.title}
              </TooltipContent>
            </Tooltip>
          </TooltipProvider>
        )}
      </div>

      <div className="flex flex-wrap items-center gap-1">
        <Gauge
          className={cn(
            "size-2.5 shrink-0",
            efforts.unknown ? "text-muted-foreground/60" : "text-muted-foreground",
          )}
        />
        {efforts.unknown ? (
          <TooltipProvider>
            <Tooltip>
              <TooltipTrigger asChild>
                <span
                  data-slot="model-capability-efforts"
                  data-unknown="true"
                  className="rounded border border-dashed border-border/70 px-1.5 py-0.5 text-[10px] text-muted-foreground"
                >
                  {efforts.text}
                </span>
              </TooltipTrigger>
              <TooltipContent className="max-w-xs text-[11px] leading-relaxed">
                {efforts.title}
              </TooltipContent>
            </Tooltip>
          </TooltipProvider>
        ) : (
          <TooltipProvider>
            <Tooltip>
              <TooltipTrigger asChild>
                <span
                  data-slot="model-capability-efforts"
                  data-unknown="false"
                  className="flex flex-wrap items-center gap-1"
                >
                  {/*
                    `items` 在 `unknown=true` 时才可能缺省，而这一支只处理已知态；
                    仍用 `?? []` 兜底是为了让类型契约与运行时行为一致 ——
                    真出现「已知但没条目」的坏数据时渲染成空的档位行，
                    而不是抛异常把整页带崩。
                  */}
                  {/*
                    **有 items 才逐档渲染；没有则回落到 text。**
                    固定单档模型（支持思考但不可选档）的 items 是**空数组**，
                    只渲染 items 会让单元格变成空白 —— 用户看到的是一个
                    什么都没有的档位行（实测踩过：断言读到 text 为 ""）。
                    它的 text 是「固定 high」这类说明，必须显示出来。
                  */}
                  {(efforts.items ?? []).length > 0 ? (
                    (efforts.items ?? []).map((effort) => (
                      <span
                        key={effort}
                        data-slot="model-capability-effort"
                        className="rounded bg-primary/10 px-1.5 py-0.5 font-mono text-[10px] text-foreground"
                      >
                        {effort}
                      </span>
                    ))
                  ) : (
                    <span
                      data-slot="model-capability-effort-fixed"
                      className="rounded border border-border/70 bg-muted/40 px-1.5 py-0.5 text-[10px] text-muted-foreground"
                    >
                      {efforts.text}
                    </span>
                  )}
                </span>
              </TooltipTrigger>
              <TooltipContent className="max-w-xs text-[11px] leading-relaxed">
                {efforts.title}
              </TooltipContent>
            </Tooltip>
          </TooltipProvider>
        )}
      </div>
    </div>
  );
}
