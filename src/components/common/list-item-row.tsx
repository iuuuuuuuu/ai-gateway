import * as React from "react";

import { cn } from "@/lib/utils";

interface ListItemRowProps {
  /** 最后一行不画分隔线（列表容器已有外边框）。 */
  isLast?: boolean;
  className?: string;
  children: React.ReactNode;
}

/** 管理列表的统一行容器，与 cc-switch 的 ListItemRow 一致。 */
export function ListItemRow({ isLast, className, children }: ListItemRowProps) {
  return (
    <div
      className={cn(
        "group flex items-center gap-3 px-4 py-2.5 transition-colors hover:bg-muted/50",
        !isLast && "border-b border-border/60",
        className,
      )}
    >
      {children}
    </div>
  );
}