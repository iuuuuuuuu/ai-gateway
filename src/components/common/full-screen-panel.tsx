import * as React from "react";
import { createPortal } from "react-dom";
import { ArrowLeft } from "lucide-react";

import { Button } from "@/components/ui/button";
import { isTextEditableTarget } from "@/lib/dom-utils";
import { cn } from "@/lib/utils";

/** 退场动画时长（ms），动画结束后再卸载节点。 */
const EXIT_DURATION = 220;

interface FullScreenPanelProps {
  isOpen: boolean;
  title: string;
  description?: string;
  onClose: () => void;
  children: React.ReactNode;
  footer?: React.ReactNode;
  /** 进入/退出动画：`fade` 淡入淡出，`slide-from-right` 自右侧滑入（用于三级下钻）。 */
  motionPreset?: "fade" | "slide-from-right";
  /** 覆盖内容区滚动容器的内边距/间距，默认 `px-4 py-5 sm:px-6 sm:py-6`。 */
  contentClassName?: string;
}

/** 退场动画期间保持挂载，动画结束后再卸载。 */
function usePresence(open: boolean, duration: number) {
  const [mounted, setMounted] = React.useState(open);

  React.useEffect(() => {
    if (open) {
      setMounted(true);
      return;
    }
    const timer = window.setTimeout(() => setMounted(false), duration);
    return () => window.clearTimeout(timer);
  }, [open, duration]);

  return mounted;
}

/**
 * 三级全屏面板：`createPortal` 到 body，覆盖整个窗口（含左侧导航栏）。
 *
 * 交互约定与 cc-switch 的 FullScreenPanel 保持一致：
 * - 顶部 64px 头部含返回按钮与标题；
 * - ESC 关闭，但输入类控件自身消费的 ESC 与 `defaultPrevented` 事件不关闭；
 * - 面板自身为独立滚动容器，不污染页面滚动位置。
 */
export function FullScreenPanel({
  isOpen,
  title,
  description,
  onClose,
  children,
  footer,
  motionPreset = "fade",
  contentClassName,
}: FullScreenPanelProps) {
  const mounted = usePresence(isOpen, EXIT_DURATION);
  const onCloseRef = React.useRef(onClose);

  React.useEffect(() => {
    onCloseRef.current = onClose;
  }, [onClose]);

  // ESC 关闭；使用冒泡阶段监听，让子组件（Radix Dialog/Select 等）优先处理。
  React.useEffect(() => {
    if (!isOpen) return;

    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (event.defaultPrevented) return;
      if (isTextEditableTarget(event.target)) return;
      event.stopPropagation();
      onCloseRef.current();
    };

    window.addEventListener("keydown", handleKeyDown, false);
    return () => window.removeEventListener("keydown", handleKeyDown, false);
  }, [isOpen]);

  if (!mounted) return null;

  const slide = motionPreset === "slide-from-right";

  return createPortal(
    <div
      role="dialog"
      aria-modal="true"
      aria-label={title}
      data-state={isOpen ? "open" : "closed"}
      className={cn(
        "fixed inset-0 z-[60] flex flex-col bg-background",
        slide
          ? "data-[state=open]:animate-in data-[state=open]:slide-in-from-right data-[state=open]:fade-in-0 data-[state=closed]:animate-out data-[state=closed]:slide-out-to-right data-[state=closed]:fade-out-0"
          : "data-[state=open]:animate-in data-[state=open]:fade-in-0 data-[state=closed]:animate-out data-[state=closed]:fade-out-0",
        "motion-reduce:animate-none",
      )}
    >
      {/* 头部：返回按钮 + 标题 */}
      <div className="flex h-16 shrink-0 items-center gap-3 border-b border-border/60 bg-background px-4 sm:px-6">
        <Button
          type="button"
          variant="outline"
          size="icon"
          className="select-none rounded-lg"
          onClick={onClose}
          aria-label="返回"
        >
          <ArrowLeft className="size-4" />
        </Button>
        <div className="min-w-0">
          <h2 className="truncate text-base font-semibold text-foreground">{title}</h2>
          {description ? (
            <p className="truncate text-xs text-muted-foreground">{description}</p>
          ) : null}
        </div>
      </div>

      {/* 内容区 */}
      <div className="min-h-0 flex-1 overflow-y-auto">
        <div className={cn("w-full space-y-5 px-4 py-5 sm:px-6 sm:py-6", contentClassName)}>
          {children}
        </div>
      </div>

      {/* 底部操作区 */}
      {footer ? (
        <div className="shrink-0 border-t border-border/60 bg-background py-3">
          <div className="flex items-center justify-end gap-3 px-4 sm:px-6">{footer}</div>
        </div>
      ) : null}
    </div>,
    document.body,
  );
}