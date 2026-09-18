import { CircleHelp, Eye, EyeOff, Globe } from "lucide-react";

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
  vision: CapabilityDisplay;
  region?: RegionHint;
  className?: string;
}

/**
 * 一个模型的能力行：**视觉**（+ 可选区域徽标）。
 *
 * ⚠ `context`（上下文窗口）与 `efforts`（思考档位）已按所有者要求撤下
 *（2026-09-18）。撤下的理由不同，两条都记在这里，避免后人「顺手加回来」：
 *
 *   · 上下文窗口：值本身可信（上游 maxInputTokens），但对「该选哪个模型」
 *     几乎没有决策价值，而它占的横向空间最大；
 *   · 思考档位：我们**给不出可靠的范围** —— 实测 supportedEfforts 不是硬范围
 *     （hy3 声明 [low,high] 却接受 medium/max/off；off 在 16 个模型里
 *     14 个可用、2 个 deepseek 拒绝）。在拿到逐模型可信清单之前，
 *     显示一组「候选」比不显示更容易误导。
 *
 * 网关**仍然**下发这些字段（supported_efforts / reasoning_range_undeclared 等），
 * 需要恢复展示时数据是现成的。
 */
export function ModelCapabilityRow({
  vision,
  region,
  className,
}: ModelCapabilityRowProps) {
  return (
    <div className={cn("flex flex-col gap-1", className)} data-slot="model-capability-row">
      <div className="flex flex-wrap items-center gap-1">
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
    </div>
  );
}
