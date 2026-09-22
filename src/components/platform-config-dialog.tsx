import type { ReactNode } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { cn } from "@/lib/utils";

// platform-config-dialog.tsx —— 平台配置的统一「配置」按钮 + 弹窗外壳。
//
// # 为什么是弹窗而不是平铺在页面上（2026-09-22 所有者要求）
//
// 所有者原话：
//
//	「这些配置应该单独做到一个按钮上,配置  然后点击弹窗进行配置」
//
// 背景：平台专属配置从设置页迁到各平台页后，如果**平铺**在页面底部，
// 会把该页原本的主角（账号列表）淹掉 —— WorkBuddy 那四张卡片加起来
// 一千三百多行，平铺时账号列表要滚很久才能看到。
//
// 弹窗把"日常看的"（账号列表）与"偶尔改的"（配置）在**空间上分开**，
// 页面保持干净，要改配置时点一下。
//
// # 为什么值得抽成公共组件
//
// WorkBuddy / Qoder / ZCode **三页都要**这个形状。各写一份的话：
//   · 弹窗宽度、滚动高度、按钮位置会各自漂移
//   · 将来要加大宽度/加「重置」按钮就得改三处
//
// # 尺寸取舍
//
//   · `max-w-3xl` —— 与设置页原本的 `max-w-3xl` 一致，迁移后
//     卡片内部布局不会因为变窄而挤成两行
//   · 内容区 `max-h-[70vh] overflow-y-auto` —— 配置项多于一屏时**在弹窗内**
//     滚动，而不是让弹窗本身溢出屏幕（后者会导致标题与关闭按钮滚出视野）
//   · 标题与底部**不参与滚动**（`shrink-0`），任何时候都看得到自己改的是哪一项
export interface PlatformConfigDialogProps {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 弹窗标题，如「WorkBuddy 配置」。 */
  title: string;
  /** 一句话说明这里配的是什么、对谁生效。 */
  description?: ReactNode;
  children: ReactNode;
  /** 内容区额外的类名（少数卡片需要更窄/更宽的容器）。 */
  contentClassName?: string;
}

export function PlatformConfigDialog({
  open,
  onOpenChange,
  title,
  description,
  children,
  contentClassName,
}: PlatformConfigDialogProps) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[85vh] min-w-0 flex-col gap-0 p-0 sm:max-w-3xl">
        {/* 标题区不滚动：配置项多时也要能一眼看到「我在改哪个平台的什么」 */}
        <DialogHeader className="shrink-0 border-b border-border px-6 py-4 text-left">
          <DialogTitle>{title}</DialogTitle>
          {description ? <DialogDescription>{description}</DialogDescription> : null}
        </DialogHeader>
        {/* ⚠ 滚动放在**这一层**，不是 DialogContent 上：
            后者会把标题与关闭按钮一起滚走。 */}
        <div className={cn("min-w-0 flex-1 overflow-y-auto px-6 py-5", contentClassName)}>
          {children}
        </div>
      </DialogContent>
    </Dialog>
  );
}

/**
 * 触发「配置」弹窗的按钮。
 *
 * 单独抽出来是为了让三个平台页的按钮**位置与外观一致** ——
 * 否则会出现"WorkBuddy 在右上角、Qoder 在最下面"这种不一致。
 */
export function PlatformConfigButton({
  onClick,
  children,
  className,
  variant = "outline",
  size = "sm",
}: {
  onClick: () => void;
  children?: ReactNode;
  className?: string;
  variant?: "default" | "outline" | "ghost" | "secondary";
  size?: "default" | "sm" | "lg" | "icon";
}) {
  return (
    <Button type="button" variant={variant} size={size} className={className} onClick={onClick}>
      {children ?? "配置"}
    </Button>
  );
}
