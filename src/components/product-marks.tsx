import { cn } from "@/lib/utils";
import workbuddyIcon from "@/assets/workbuddy-official-icon.png";
import codebuddyCnIdeIcon from "@/assets/codebuddy-cn-ide-icon.png";

const appIconUrl = `${import.meta.env.BASE_URL}icon-transparent.png`;

interface MarkProps {
  size?: number;
  className?: string;
}

/**
 * WorkBuddy 官方应用图标（从 WorkBuddy.app 的 icon.icns 提取）。
 * 与 CodeBuddy IDE 图标同为标准 macOS app icon 风格（约 10% 透明边距），
 * 放大 118% 居中裁掉透明圈后与 CodeBuddy 系列图标视觉一致。
 */
export function WorkBuddyMark({ size = 32, className }: MarkProps) {
  return (
    <span
      aria-hidden
      className={cn("relative inline-flex shrink-0 overflow-hidden rounded-[22%]", className)}
      style={{ width: size, height: size }}
    >
      <img
        src={workbuddyIcon}
        alt=""
        className="absolute left-1/2 top-1/2 size-[118%] max-w-none -translate-x-1/2 -translate-y-1/2 object-cover"
      />
    </span>
  );
}

/** 应用自身的透明角色图标；桌面安装图标仍使用 public/icon.png。 */
export function AppIconMark({ size = 32, className }: MarkProps) {
  return (
    <span
      aria-hidden
      className={cn("inline-flex shrink-0", className)}
      style={{ width: size, height: size }}
    >
      <img src={appIconUrl} alt="" className="size-full object-contain" />
    </span>
  );
}

export function CodeBuddyMark({ size = 32, className }: MarkProps) {
  const icon = Math.max(10, Math.round(size));
  return (
    <span
      aria-hidden
      className={cn(
        "inline-flex shrink-0 items-center justify-center rounded-[22%] border border-white/10 bg-zinc-950 text-zinc-50 shadow-sm",
        className,
      )}
      style={{ width: size, height: size, fontSize: icon }}
    >
      <svg
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="2.2"
        strokeLinecap="round"
        strokeLinejoin="round"
        className="size-[1em]"
      >
        <path d="M4.4 7.4 10.4 12 4.4 16.6" />
        <path d="M13 16.6h7" />
      </svg>
    </span>
  );
}

/**
 * CodeBuddy IDE（桌面客户端）官方应用图标。
 * 源图四周自带约 9% 透明边距：正方形图 + object-cover 不会触发任何缩放，
 * 必须先把图放大到 122% 再居中裁剪，才能把透明圈裁掉并与 WorkBuddy 的
 * 全幅 logo 达到同样的视觉大小（裁剪仅落在透明边距上，几乎不伤画面）。
 */
export function CodeBuddyCnIdeMark({ size = 32, className }: MarkProps) {
  return (
    <span
      aria-hidden
      className={cn("relative inline-flex shrink-0 overflow-hidden", className)}
      style={{ width: size, height: size }}
    >
      <img
        src={codebuddyCnIdeIcon}
        alt=""
        className="absolute left-1/2 top-1/2 size-[122%] max-w-none -translate-x-1/2 -translate-y-1/2 object-cover"
      />
    </span>
  );
}

export function StatusDot({ on, className }: { on: boolean; className?: string }) {
  return (
    <span
      aria-hidden
      className={cn("size-1.5 shrink-0 rounded-full", on ? "bg-primary" : "bg-muted-foreground/35", className)}
    />
  );
}

/**
 * Trae 系（Trae Work / Trae）图标。
 *
 * 用矢量字形而不是位图：官方图标是受版权保护的美术资源，本仓库不内置；
 * 且两个 Trae 应用只需按配色区分，矢量在任何缩放下都清晰。
 */
export function TraeMark({
  size = 32,
  className,
  variant = "work",
}: MarkProps & { variant?: "work" | "cn" }) {
  return (
    <span
      aria-hidden
      className={cn(
        "inline-flex shrink-0 items-center justify-center rounded-[22%] text-white shadow-sm",
        variant === "work"
          ? "bg-gradient-to-br from-indigo-500 to-violet-600"
          : "bg-gradient-to-br from-sky-500 to-cyan-600",
        className,
      )}
      style={{ width: size, height: size }}
    >
      <svg viewBox="0 0 24 24" fill="none" className="size-[62%]" aria-hidden="true">
        <path d="M4.5 7h15" stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" />
        <path d="M12 7v11" stroke="currentColor" strokeWidth="2.4" strokeLinecap="round" />
      </svg>
    </span>
  );
}

/** 豆包图标。 */
export function DoubaoMark({ size = 32, className }: MarkProps) {
  return (
    <span
      aria-hidden
      className={cn(
        "inline-flex shrink-0 items-center justify-center rounded-[22%] bg-gradient-to-br from-blue-500 to-indigo-600 text-white shadow-sm",
        className,
      )}
      style={{ width: size, height: size }}
    >
      <svg viewBox="0 0 24 24" fill="none" className="size-[62%]" aria-hidden="true">
        <path
          d="M12 4.2c-3.6 0-6.4 2.5-6.4 5.9 0 1.9.9 3.4 2.3 4.5v2.9l2.7-1.6c.45.1.92.15 1.4.15 3.6 0 6.4-2.5 6.4-5.95S15.6 4.2 12 4.2Z"
          stroke="currentColor"
          strokeWidth="1.9"
          strokeLinejoin="round"
        />
      </svg>
    </span>
  );
}

/**
 * Qoder 图标。
 *
 * 用「尖括号 + 中心点」表达「代码 + 智能体」，与 Qoder 的定位一致。
 * 配色用 Qoder 品牌的深紫蓝渐变，与豆包（蓝紫）、Trae（深灰）在同排时
 * 能一眼区分 —— 三个产品图标并列时靠**色相**而非细节区分，小尺寸下更可靠。
 *
 * 刻意**不**去抓官方 logo 图片：本仓库是公开仓库，引入第三方商标资源有
 * 授权问题；且现有各产品的 mark 也都是自绘几何图形（见 WorkBuddyMark 等），
 * 统一自绘比混用图片更一致。
 */
export function QoderMark({ size = 32, className }: MarkProps) {
  return (
    <span
      aria-hidden
      className={cn(
        "inline-flex shrink-0 items-center justify-center rounded-[22%] bg-gradient-to-br from-violet-500 to-purple-700 text-white shadow-sm",
        className,
      )}
      style={{ width: size, height: size }}
    >
      <svg viewBox="0 0 24 24" fill="none" className="size-[62%]" aria-hidden="true">
        <path
          d="M8.6 7.4 4.2 12l4.4 4.6M15.4 7.4 19.8 12l-4.4 4.6"
          stroke="currentColor"
          strokeWidth="1.9"
          strokeLinecap="round"
          strokeLinejoin="round"
        />
        <circle cx="12" cy="12" r="1.85" fill="currentColor" />
      </svg>
    </span>
  );
}
