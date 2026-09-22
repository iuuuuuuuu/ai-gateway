import * as React from "react";
import * as DropdownMenuPrimitive from "@radix-ui/react-dropdown-menu";
import { Check as CheckIcon } from "lucide-react";

import { cn } from "@/lib/utils";

function DropdownMenu(props: React.ComponentProps<typeof DropdownMenuPrimitive.Root>) {
  return <DropdownMenuPrimitive.Root data-slot="dropdown-menu" {...props} />;
}

function DropdownMenuTrigger(props: React.ComponentProps<typeof DropdownMenuPrimitive.Trigger>) {
  return <DropdownMenuPrimitive.Trigger data-slot="dropdown-menu-trigger" {...props} />;
}

function DropdownMenuContent({ className, sideOffset = 6, ...props }: React.ComponentProps<typeof DropdownMenuPrimitive.Content>) {
  return (
    <DropdownMenuPrimitive.Portal>
      <DropdownMenuPrimitive.Content
        data-slot="dropdown-menu-content"
        sideOffset={sideOffset}
        className={cn(
          "z-50 min-w-36 rounded-lg border bg-popover p-1.5 text-popover-foreground shadow-[0_10px_30px_rgba(15,23,42,.12)]",
          // ⚠⚠ 高度自适应 + 可滚动（2026-09-22 修）。
          //
          // # 所有者现场
          //
          //	「workbuddy账号点击菜单后超出屏幕,无法向下滚动」
          //
          // # 根因：这里原本是 `overflow-hidden`
          //
          // `overflow-hidden` 的语义是**裁掉**溢出内容，而不是让人滚。
          // 账号菜单本轮新增了「全部成长任务」分组（动态拉取，实测 19 项），
          // 菜单长到 20+ 项、高过一屏 —— 于是超出部分**既看不到也滚不到**。
          //
          // # 为什么 Radix 的碰撞检测没兜住
          //
          // Radix 会把菜单**挪**进视口（翻转/位移），但它只保证
          // "菜单框在视口内"，**不负责让内容可滚动**。
          // 内容高于视口时，超出部分就是不可达的。
          //
          // # 正确做法：用 Radix 提供的 CSS 变量
          //
          // Radix 在 Content 上挂 `--radix-dropdown-menu-content-available-height`
          // （视口内实际可用高度，已扣掉碰撞边距）。用它当 max-height
          // 比写死 `70vh` 精确：菜单靠近屏幕底边时会自动变矮，
          // 而不是先撑到 70vh 再被裁。
          //
          // ⚠ `overflow-y-auto`（而非 `overflow-auto`）：只让纵向滚，
          // 横向保持不滚（菜单项都是 truncate 的，不需要横向滚动）。
          //
          // ⚠ 这修的是**全站所有下拉菜单**（本文件是共用组件）——
          // 那个缺陷本来就不只影响账号卡片，只是账号菜单最长、最先撞上。
          "max-h-[var(--radix-dropdown-menu-content-available-height)] overflow-y-auto overflow-x-hidden",
          "data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0 data-[state=closed]:zoom-out-95 data-[state=open]:zoom-in-95",
          "data-[side=bottom]:slide-in-from-top-1 data-[side=left]:slide-in-from-right-1 data-[side=right]:slide-in-from-left-1 data-[side=top]:slide-in-from-bottom-1",
          className,
        )}
        {...props}
      />
    </DropdownMenuPrimitive.Portal>
  );
}

function DropdownMenuItem({ className, inset, ...props }: React.ComponentProps<typeof DropdownMenuPrimitive.Item> & { inset?: boolean }) {
  return (
    <DropdownMenuPrimitive.Item
      data-slot="dropdown-menu-item"
      data-inset={inset}
      className={cn(
        "relative flex cursor-default select-none items-center gap-2 rounded-md px-2.5 py-2 text-sm outline-none transition-colors",
        "focus:bg-accent focus:text-accent-foreground data-[disabled]:pointer-events-none data-[disabled]:opacity-50 data-[inset=true]:pl-8 [&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0",
        className,
      )}
      {...props}
    />
  );
}

/**
 * 带勾选框的菜单项（可多选列表用）。
 *
 * 为什么按 UI 规范包一层 Radix 而不是自己拿 DropdownMenuItem + Checkbox 拼：
 * 拼出来的版本会丢掉两件**免费但难做对**的东西 ——
 *  1. `aria-checked` / `role="menuitemcheckbox"` 语义（屏幕阅读器据此报「已勾选」）
 *  2. 菜单打开时键盘可达、Enter/Space 切换、以及 Radix 的 typeahead 首字母跳转
 * 自定义组合要么漏掉其一，要么得把 Radix 内部逻辑再写一遍。
 *
 * 勾选态用 Radix 的 ItemIndicator 而不是自己塞一个图标：它由
 * `data-state=checked` 驱动，与 `checked` prop 天然同步，不需要在调用方维护映射。
 */
function DropdownMenuCheckboxItem({
  className,
  children,
  checked,
  ...props
}: React.ComponentProps<typeof DropdownMenuPrimitive.CheckboxItem>) {
  return (
    <DropdownMenuPrimitive.CheckboxItem
      data-slot="dropdown-menu-checkbox-item"
      checked={checked}
      className={cn(
        "relative flex cursor-default select-none items-center gap-2 rounded-md px-2.5 py-2 text-sm outline-none transition-colors",
        "focus:bg-accent focus:text-accent-foreground data-[disabled]:pointer-events-none data-[disabled]:opacity-50 [&_svg]:pointer-events-none [&_svg]:size-4 [&_svg]:shrink-0",
        className,
      )}
      {...props}
    >
      <span className="flex size-4 shrink-0 items-center justify-center rounded-[4px] border border-input">
        <DropdownMenuPrimitive.ItemIndicator>
          <CheckIcon className="size-3.5" />
        </DropdownMenuPrimitive.ItemIndicator>
      </span>
      {children}
    </DropdownMenuPrimitive.CheckboxItem>
  );
}

function DropdownMenuSeparator({ className, ...props }: React.ComponentProps<typeof DropdownMenuPrimitive.Separator>) {
  return <DropdownMenuPrimitive.Separator data-slot="dropdown-menu-separator" className={cn("-mx-1 my-1 h-px bg-border", className)} {...props} />;
}

/**
 * 菜单分组标题。
 *
 * 为什么需要它：账号菜单里现在既有「只作用于这个号」的动作，也有
 * 「触发一整轮、作用于全部账号」的动作。两者混在一起排成一条竖列时，
 * 用户会默认每一项都只影响当前卡片 —— 点完却动了所有号，属于误导。
 * 分组标题是区分它们最轻的手段（不额外增加点击层级）。
 */
function DropdownMenuLabel({ className, ...props }: React.ComponentProps<typeof DropdownMenuPrimitive.Label>) {
  return (
    <DropdownMenuPrimitive.Label
      data-slot="dropdown-menu-label"
      className={cn("px-2.5 py-1.5 text-[11px] font-semibold text-muted-foreground", className)}
      {...props}
    />
  );
}

export {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
};
