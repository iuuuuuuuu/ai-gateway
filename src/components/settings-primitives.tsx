import type { ReactElement, ReactNode } from "react";

import { Card } from "@/components/ui/card";
import { Label } from "@/components/ui/label";
import { DemoAction } from "@/components/demo-action";
import { cn } from "@/lib/utils";

// settings-primitives.tsx —— 设置页与**各平台页**共用的分组/行原件。
//
// # 为什么单独抽出来（2026-09-22 所有者要求）
//
// 所有者原话：
//
//	「设置页面的每个平台的配置,迁移到每个平台自己的页面去,不要留在设置页面
//	  设置页面只保留除了接入平台的 其他配置」
//
// 于是「平台专属配置」要搬到 WorkBuddy / Qoder / ZCode 各自的页面。
// 那三页原本没有这套视觉原件（分组标题 + 卡片 + 行），
// 若不抽出来，就只能：
//
//   · 把设置页的实现**复制三份**（改一处要同步四处，必然漂移），或
//   · 让平台页反向 import 设置页的内部函数（那会把设置页变成公共依赖）
//
// 两者都不可接受，故提到这里。**组件本身没改任何行为** ——
// 只是从 `SettingsPage.tsx` 原样搬出，让三页都能用同一套原件。

export interface SettingsGroupProps {
  id: string;
  title: string;
  children: ReactNode;
}

/**
 * 一组设置：小标题 + 圆角卡片。
 *
 * ⚠ `id` 会渲染成 `aria-labelledby` 的目标，各页内必须唯一 ——
 * 迁移过来的卡片保留了原 `settings-*` 前缀（如 `settings-auto-checkin`），
 * 以便"这个卡片原来在哪"仍可追溯，也避免跨页碰撞。
 */
export function SettingsGroup({ id, title, children }: SettingsGroupProps) {
  return (
    <section className="min-w-0 space-y-2.5" aria-labelledby={id}>
      <div className="px-1">
        <h2 id={id} className="text-[13px] font-medium leading-5">
          {title}
        </h2>
      </div>
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">{children}</Card>
    </section>
  );
}

export function SettingsRow({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <div
      className={cn(
        "mx-4 flex min-w-0 items-center justify-between gap-3 border-b border-border/50 px-0 py-2.5 sm:mx-5",
        className,
      )}
    >
      {children}
    </div>
  );
}

export interface SettingsFieldRowProps {
  label: ReactNode;
  description?: ReactNode;
  htmlFor?: string;
  children: ReactNode;
  className?: string;
  /**
   * 该行是否属于「会改动外部状态的操作」（签到、领取、重启…）。
   *
   * 演示模式下这些会被 `DemoAction` 拦下 —— 与设置页原本的行为一致。
   */
  operational?: boolean;
}

export function SettingsFieldRow({
  label,
  description,
  htmlFor,
  children,
  className,
  operational = false,
}: SettingsFieldRowProps) {
  return (
    <SettingsRow className={cn("flex-col items-stretch gap-2 sm:flex-row sm:items-center", className)}>
      <div className="min-w-0 flex-1">
        {htmlFor ? (
          <Label htmlFor={htmlFor} className="text-[13px] leading-4">
            {label}
          </Label>
        ) : (
          <div className="text-[13px] font-medium leading-4">{label}</div>
        )}
        {description && (
          <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">{description}</p>
        )}
      </div>
      <div className="flex min-w-0 w-full shrink-0 justify-end sm:w-auto">
        {operational ? <DemoAction className="w-full sm:w-auto">{children as ReactElement}</DemoAction> : children}
      </div>
    </SettingsRow>
  );
}
