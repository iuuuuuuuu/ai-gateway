import { cn } from "@/lib/utils";
import workbuddyIcon from "@/assets/workbuddy-official-icon.png";
import codebuddyCnIdeIcon from "@/assets/codebuddy-cn-ide-icon.png";
import qoderIcon from "@/assets/qoder-official-icon.png";
import zcodeIcon from "@/assets/zcode-official-icon.png";

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
 * Qoder 图标 —— **官方应用图标**（2026-09-22 改为官方资源）。
 *
 * # 为什么改了（此前是自绘的「尖括号 + 中心点」）
 *
 * 所有者原话：
 *
 *	「zcode后面不还是一个闪电小卡片么?还不是官方的logo  qoder也一样」
 *
 * 此前的注释写着「刻意不抓官方 logo：本仓库是公开仓库，引入第三方商标
 * 资源有授权问题」—— **那个理由是错的/过期的**：仓库里早就有官方资源
 *（`workbuddy-official-icon.png`、`agent-icons/zcode.png`），
 * 本组件自身也在用 `workbuddyIcon`。既然 WorkBuddy 用的是官方图，
 * 只把 Qoder/ZCode 留在自绘就是不统一 —— 而且自绘的实心渐变块
 * 当水印时会显出一整块色斑（见 `product-account-card.tsx` 的说明）。
 *
 * # 来源
 *
 * 从**本机已安装的 Qoder 官方客户端**提取：
 *
 *	%LOCALAPPDATA%\Programs\Qoder CN\.qoder-versions\<ver>\
 *	  resources\application-icons\qoder-light.png   (1024×1024)
 *
 * ⚠ 用的是 `qoder-light.png` 而非 `qoder-light-windows.png`：
 * 后者在右上角带一个「CN」折角标（那是国服客户端的标记），
 * 而我们的账号可能是国际版 —— 带 CN 角标会对国际版用户形成误导。
 * `qoder-light.png` 只有图标本体与圆角底。
 */
export function QoderMark({ size = 32, className }: MarkProps) {
  return (
    <span
      aria-hidden
      className={cn("relative inline-flex shrink-0 overflow-hidden rounded-[22%]", className)}
      style={{ width: size, height: size }}
    >
      {/* 与 WorkBuddyMark 同款：官方 app icon 自带约 10% 透明边距，
          放大 118% 居中裁掉透明圈后与其它图标视觉尺寸一致。 */}
      <img
        src={qoderIcon}
        alt=""
        className="absolute left-1/2 top-1/2 size-[118%] max-w-none -translate-x-1/2 -translate-y-1/2 object-cover"
      />
    </span>
  );
}

/**
 * ZCode 图标 —— **官方应用图标**（2026-09-22 改为官方资源）。
 *
 * # 为什么改了（此前是自绘的「闪电」）
 *
 * 所有者原话：
 *
 *	「zcode后面不还是一个闪电小卡片么?还不是官方的logo  qoder也一样」
 *
 * 此前的注释写着「不抓官方 logo：公开仓库引入第三方商标有授权问题」——
 * **那个理由是错的/过期的**：`src/assets/agent-icons/zcode.png`
 * 就是官方图标，而且**智能体管理页早就在用它**（`AgentsPage.tsx`）。
 * 同一个产品在仓库里有两套图标（一页自绘、一页官方）本身就是不一致。
 *
 * # 来源
 *
 * `src/assets/zcode-official-icon.png` —— 与 `agent-icons/zcode.png`
 * 是同一个文件（此处复制一份是为了让 `assets/` 下的官方图标命名一致：
 * `workbuddy-official-icon` / `qoder-official-icon` / `zcode-official-icon`）。
 */
export function ZcodeMark({ size = 32, className }: MarkProps) {
  return (
    <span
      aria-hidden
      className={cn("relative inline-flex shrink-0 overflow-hidden rounded-[22%]", className)}
      style={{ width: size, height: size }}
    >
      <img
        src={zcodeIcon}
        alt=""
        className="absolute left-1/2 top-1/2 size-[118%] max-w-none -translate-x-1/2 -translate-y-1/2 object-cover"
      />
    </span>
  );
}
