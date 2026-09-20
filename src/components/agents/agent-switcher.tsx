import * as React from "react";
import { Monitor, MoreHorizontal, Terminal } from "lucide-react";

import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { cn } from "@/lib/utils";

/** 角标：Claude Code 用终端、Claude Desktop 用显示器区分同一品牌图标。 */
export type AgentSwitcherBadge = "terminal" | "monitor";

export interface AgentSwitcherItem {
  id: string;
  label: string;
  icon: string;
  badge?: AgentSwitcherBadge;
  /** 次要说明，悬停时作为 title 的补充。 */
  hint?: string;
}

interface AgentSwitcherProps {
  items: AgentSwitcherItem[];
  activeId: string;
  onSelect: (id: string) => void;
  /** 溢出菜单的无障碍标题。 */
  moreLabel?: string;
  className?: string;
}

const BADGE_ICON: Record<AgentSwitcherBadge, typeof Terminal> = {
  terminal: Terminal,
  monitor: Monitor,
};

/** 图标 + 角标（与 cc-switch AppSwitcher 的 AppGlyph 视觉一致）。 */
function AgentGlyph({ item, active }: { item: AgentSwitcherItem; active: boolean }) {
  const BadgeIcon = item.badge ? BADGE_ICON[item.badge] : null;
  return (
    <span className="relative inline-flex shrink-0">
      <img src={item.icon} alt="" className="size-5 object-contain" />
      {BadgeIcon ? (
        <span
          aria-hidden="true"
          className={cn(
            "absolute -bottom-0.5 -right-0.5 flex size-[11px] items-center justify-center rounded-[3px] border",
            active
              ? "border-border bg-background text-foreground"
              : "border-background bg-muted text-muted-foreground group-hover:bg-background group-hover:text-foreground",
          )}
        >
          <BadgeIcon className="size-[8px]" strokeWidth={2.5} />
        </span>
      ) : null}
    </span>
  );
}

/**
 * 一级菜单：客户端分段切换控件。
 *
 * 直接套用 cc-switch `AppSwitcher` 的交互与视觉：
 * - 容器 `bg-muted rounded-xl p-1 gap-1`，按钮 `px-3 h-8 rounded-md`；
 * - 激活项 `bg-background shadow-sm`；
 * - 空间不足时按可用宽度自动收纳进「更多」Popover，激活项始终占位可见。
 */
export function AgentSwitcher({
  items,
  activeId,
  onSelect,
  moreLabel = "更多",
  className,
}: AgentSwitcherProps) {
  const rootRef = React.useRef<HTMLDivElement>(null);
  const [moreOpen, setMoreOpen] = React.useState(false);
  const [visibleCount, setVisibleCount] = React.useState(items.length);

  const itemCount = items.length;

  // 宽度必须取父弹性槽而非自身：自身宽度随可见数量变化，用它做输入会形成
  // 收起 → 变窄 → 再收起的反馈循环。
  React.useLayoutEffect(() => {
    const root = rootRef.current;
    const slot = root?.parentElement;
    if (!root || !slot) return;

    const compute = () => {
      const sample = root.querySelector<HTMLElement>("button[data-agent-tab]");
      if (!sample) return;
      const itemWidth = sample.offsetWidth;
      // jsdom 或未完成布局时 offsetWidth 为 0，保持全部可见
      if (itemWidth <= 0) return;
      const rootStyle = window.getComputedStyle(root);
      const gap = Number.parseFloat(rootStyle.columnGap) || 0;
      const padding =
        (Number.parseFloat(rootStyle.paddingLeft) || 0) +
        (Number.parseFloat(rootStyle.paddingRight) || 0);
      const available = slot.clientWidth;
      const widthAll = padding + itemCount * itemWidth + (itemCount - 1) * gap;
      if (widthAll <= available) {
        setVisibleCount(itemCount);
        return;
      }
      // 「更多」按钮与应用按钮同宽（同 padding + 同尺寸图标）
      const fit = Math.floor((available - padding - itemWidth) / (itemWidth + gap));
      setVisibleCount(Math.max(1, Math.min(itemCount - 1, fit)));
    };

    compute();
    const observer = new ResizeObserver(compute);
    observer.observe(slot);
    return () => observer.disconnect();
  }, [itemCount]);

  const visibleList = items.slice(0, Math.max(1, Math.min(visibleCount, itemCount)));
  // 激活项被收进溢出区时顶替最后一个可见位，保证始终可点亮。
  const activeItem = items.find((item) => item.id === activeId);
  if (activeItem && !visibleList.some((item) => item.id === activeId)) {
    visibleList[visibleList.length - 1] = activeItem;
  }
  const visibleIds = new Set(visibleList.map((item) => item.id));
  const overflowList = items.filter((item) => !visibleIds.has(item.id));

  const buttonClass = (active: boolean) =>
    cn(
      "group inline-flex h-8 cursor-pointer items-center gap-2 rounded-md px-3 text-sm font-medium transition-all duration-200 outline-none",
      "focus-visible:ring-2 focus-visible:ring-ring/50",
      active
        ? "bg-background text-foreground shadow-sm"
        : "text-muted-foreground hover:bg-background/50 hover:text-foreground",
    );

  return (
    <div
      ref={rootRef}
      role="tablist"
      aria-label="智能体客户端"
      className={cn("inline-flex items-center gap-1 rounded-xl bg-muted p-1", className)}
    >
      {visibleList.map((item) => {
        const active = item.id === activeId;
        return (
          <button
            key={item.id}
            type="button"
            data-agent-tab
            role="tab"
            aria-selected={active}
            title={item.hint ? `${item.label} · ${item.hint}` : item.label}
            onClick={() => onSelect(item.id)}
            className={buttonClass(active)}
          >
            <AgentGlyph item={item} active={active} />
            <span className="hidden max-w-[9rem] truncate lg:inline">{item.label}</span>
          </button>
        );
      })}

      {overflowList.length > 0 ? (
        <Popover open={moreOpen} onOpenChange={setMoreOpen}>
          <PopoverTrigger asChild>
            <button
              type="button"
              title={moreLabel}
              aria-label={moreLabel}
              className={cn(buttonClass(false), moreOpen && "bg-background text-foreground shadow-sm")}
            >
              <MoreHorizontal className="size-5 shrink-0" />
            </button>
          </PopoverTrigger>
          <PopoverContent side="bottom" align="end" className="w-60 p-1">
            {overflowList.map((item) => (
              <button
                key={item.id}
                type="button"
                onClick={() => {
                  setMoreOpen(false);
                  onSelect(item.id);
                }}
                className={cn(
                  "group flex w-full cursor-pointer items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm font-medium transition-colors",
                  item.id === activeId
                    ? "bg-accent text-accent-foreground"
                    : "text-muted-foreground hover:bg-accent hover:text-accent-foreground",
                )}
              >
                <AgentGlyph item={item} active={item.id === activeId} />
                <span className="truncate">{item.label}</span>
              </button>
            ))}
          </PopoverContent>
        </Popover>
      ) : null}
    </div>
  );
}