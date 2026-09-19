import { useEffect, useRef, type ReactNode } from "react";

import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { cn } from "@/lib/utils";

/**
 * 日志「智能自动滚动」容器 + 级别着色。
 *
 * ---------------------------------------------------------------------------
 * 为什么需要「智能」而不是无脑 `scrollTop = scrollHeight`
 * ---------------------------------------------------------------------------
 *
 * 无脑滚到底会在用户**向上翻阅历史**时把视口拽回最新处 —— 日志越长越难读，
 * 等于逼用户「先关掉自动滚动、看完再打开」。本组件的做法是：只在用户**本来
 * 就停在最新一端**时才跟随新内容滚动；一旦用户主动往历史方向滚，就停手不再打扰。
 * 这是本改进的全部价值所在，因此下面的「是否贴边」判定是核心逻辑，不是细节。
 *
 * 阈值取 24px（与参考实现 `app.js:373-383` 的 `atEnd` 一致）：滚动位置是浮点、
 * 且不同缩放下 `scrollHeight - clientHeight - scrollTop` 常残留 1~2px 误差，
 * 用 `=== 0` 判定会导致「明明在最新处却认为用户滚走了」，自动滚动随机失灵。
 *
 * ---------------------------------------------------------------------------
 * 为什么需要 `anchor`（本组件与参考实现的唯一实质差异）
 * ---------------------------------------------------------------------------
 *
 * 参考实现是 `box.scrollTop = box.scrollHeight`，隐含「最新在**底部**」这一前提。
 * 但本仓库的两份日志**都是最新在顶部**：
 *   · 签到日志：后端按插入顺序返回（旧 → 新），前端 `[...logs].reverse()` 显示；
 *   · 轮换日志：后端 `rotate_logs()` 已 `reverse()`（新 → 旧），前端直接渲染。
 * 若照抄 `scrollHeight`，每次「自动滚动」都会把视口送到**最旧**的一条 —— 正好
 * 与需求相反，且看起来像开关坏了。
 *
 * 因此这里把「最新在哪一端」显式参数化为 `anchor`，两种方向都实现：
 *   · `anchor: "start"`（最新在顶部，本仓库当前用法）→ 跟随 = `scrollTop = 0`；
 *   · `anchor: "end"`（最新在底部，如流式追加的代理日志）→ 跟随 = 滚到底。
 * 这样既不改变用户已经习惯的显示顺序（新 → 旧），又能让跟随语义正确。
 *
 * ---------------------------------------------------------------------------
 * 级别着色为什么用**内容正则**而不是只看结构化字段
 * ---------------------------------------------------------------------------
 *
 * 日志文本里混着两类信息：结构化状态（`result` / `action`）与**自由文本**
 *（`error` / `reason` / 代理原始行）。前者已有权威的 tone 判定，后者只能靠内容 ——
 * 这正是参考实现 `const lvl = /error|失败|错误/.test(e.text) ? 'e' : …` 的用法。
 * 本文件把内容判定抽成 `logLevelOf()`，供自由文本与纯文本行共用；
 * 结构化字段的 tone 判定仍由调用方保留（它比正则更准）。
 */

/** 日志级别（由内容判定得出）。 */
export type LogLevel = "error" | "warn" | "info";

/**
 * 按内容判定日志级别。
 *
 * 顺序**必须**是先 error 再 warn：一句话里同时出现「失败」与「冷却」
 *（如「冷却后仍然失败」）时，它首先是**错误** —— 反过来的话最严重的那类
 * 反而被标成琥珀，用户扫视时会漏掉。
 *
 * 用 `i` 标志而不是只匹配小写：日志里大小写混杂（`ERROR` / `Error` / `WARN`），
 * 只认小写会漏掉大写形式。中文没有大小写，不受影响。
 */
export function logLevelOf(text: string): LogLevel {
  if (/error|失败|错误/i.test(text)) return "error";
  if (/warn|冷却|熔断/i.test(text)) return "warn";
  return "info";
}

/**
 * 级别 → 文字颜色。
 *
 * 每个级别都带**暗色变体**：亮色的 `-600` 档在暗色卡片底上偏暗（状态列早已
 * 改用 `text-amber-600 dark:text-amber-400` 这套写法），只给亮色值会让暗色
 * 主题下的告警读起来发闷。红色用主题 token `--destructive`（两套主题各有取值，
 * 自动适配），与全站「错误 / 破坏性」的语义色一致。
 */
export const LOG_LEVEL_CLASS: Record<LogLevel, string> = {
  error: "text-destructive",
  warn: "text-amber-600 dark:text-amber-400",
  info: "text-muted-foreground",
};

/** 结构化 tone → 文字颜色（保留调用方更权威的判定）。 */
export const LOG_TONE_CLASS: Record<"error" | "warning" | "success", string> = {
  error: "text-destructive",
  warning: "text-amber-600 dark:text-amber-400",
  success: "text-emerald-600 dark:text-emerald-400",
};

/** 滚动容器可选的固定高度档位（沿用改动前各自的 `max-h-*`，观感不变）。 */
const HEIGHT_CLASS = {
  /** 签到 / 轮换日志（原 `max-h-64`）。 */
  md: "max-h-64",
  /** 代理日志（原 `max-h-40`）。 */
  sm: "max-h-40",
} as const;

/** 「最新内容」在哪一端。 */
export type LogAnchor = "start" | "end";

interface LogTailProps {
  /**
   * 内容版本号：**变化即视为「有新内容」**，据此决定要不要跟随滚动。
   *
   * 为什么不让容器自己用 `MutationObserver` 或「每次渲染都滚」：
   *  - 每次渲染都滚会连「展开/收起、切主题」这类与日志无关的重渲染也算进去，
   *    在用户正看历史时把视口拽走；
   *  - `MutationObserver` 要额外管理观察器生命周期，而这里的内容是
   *    受控数组，调用方**本来就知道**它什么时候变了，直接传进来最省也最准。
   */
  revision: string | number;
  /** 自动滚动总开关（由外部的「自动滚动：开/关」开关控制）。 */
  autoScroll: boolean;
  /** 最新内容在哪一端，见文件头说明。 */
  anchor: LogAnchor;
  children: ReactNode;
  /** 高度档位，默认 `md`（`max-h-64`）。 */
  size?: keyof typeof HEIGHT_CLASS;
  className?: string;
  /** 滚动区域的读屏名称（日志列表对读屏用户是一大块文本，需要名字）。 */
  label?: string;
}

/**
 * 智能自动滚动容器。
 *
 * 挂载时（`revision` 首次生效）会滚到最新一端，让用户直接看到最新一条 ——
 * 这正是「tail」应有的初始状态。
 */
export function LogTail({
  revision,
  autoScroll,
  anchor,
  children,
  size = "md",
  className,
  label,
}: LogTailProps) {
  const boxRef = useRef<HTMLDivElement | null>(null);

  /**
   * 用户当前是否「停在最新一端」。
   *
   * 用 ref 而不是 state 存**判定结果**：它只被滚动事件与 effect 读写，
   * 若放进 state，每次滚动都会触发一次重渲染 —— 而滚动是高频事件，
   * 白白让整个日志列表重渲染。这里没有任何 UI 依赖它，ref 才是对的。
   */
  const atLatestRef = useRef(true);

  /** 自动滚动由关转开时，需要强制回到最新（见下方 effect）。 */
  const wasAutoScrollRef = useRef(autoScroll);

  /**
   * 记录用户是否停在最新端。**只在用户主动滚动时更新**（onScroll），
   * 不在程序滚动时更新 —— 程序滚到最新端后本来就在最新端，判定结果不变，
   * 但多读一次 `scrollTop` 反而会在某些浏览器上触发同步布局（强制 reflow）。
   *
   * `anchor` 决定「最新端」是哪一端，带 24px 容差（理由见文件头）。
   */
  function handleScroll() {
    const box = boxRef.current;
    if (!box) return;
    atLatestRef.current =
      anchor === "start"
        ? box.scrollTop <= 24
        : box.scrollTop + box.clientHeight >= box.scrollHeight - 24;
  }

  // 新内容到达：**仅当用户本来就停在最新端**时才跟随滚动。
  //
  // 依赖只放 `revision` / `autoScroll` / `anchor` 三个**原始值**（都是 string /
  // boolean，比较稳定）。刻意不放 `handleScroll` 之类的函数：它每次渲染都会重建，
  // 放进依赖会让 effect 每次都跑 —— 那就退化成「每次渲染都滚」，正是要避免的行为。
  //
  // 对 `anchor="start"`（最新在顶）而言，这一句多半是**幂等的**：新条目插在顶部
  // 时浏览器本来就会保持 `scrollTop`，停在 0 的用户自动就看到最新一条。它真正
  // 起作用的是「用户滚下去看历史、又手动滚回顶部附近」之后再来的新内容。
  // 用户停在中间读历史时，本条与 `overflow-anchor`（Chromium 默认开启）共同
  // 保证他的阅读位置不被新内容顶走。
  useEffect(() => {
    const box = boxRef.current;
    if (!box) return;
    if (!autoScroll) return;
    if (!atLatestRef.current) return;
    box.scrollTop = anchor === "start" ? 0 : box.scrollHeight;
  }, [revision, autoScroll, anchor]);

  // 自动滚动由关转开：用户刚刚明确要求「跟随最新」，因此强制回到最新端，
  // 而不是等他再滚一次才生效（否则会看起来像开关坏了）。
  useEffect(() => {
    if (autoScroll && !wasAutoScrollRef.current) {
      atLatestRef.current = true;
      const box = boxRef.current;
      if (box) box.scrollTop = anchor === "start" ? 0 : box.scrollHeight;
    }
    wasAutoScrollRef.current = autoScroll;
  }, [autoScroll, anchor]);

  return (
    <div
      ref={boxRef}
      onScroll={handleScroll}
      role="log"
      aria-label={label}
      className={cn("overflow-y-auto pr-1", HEIGHT_CLASS[size], className)}
    >
      {children}
    </div>
  );
}

/**
 * 日志区块的标题行：标题 + 「自动滚动」开关。
 *
 * 开关缺省**开**（tail 场景下用户要的就是跟随最新）。
 */
export function LogTailHeader({
  title,
  autoScroll,
  onAutoScrollChange,
  switchId,
}: {
  title: ReactNode;
  autoScroll: boolean;
  onAutoScrollChange: (next: boolean) => void;
  /** 开关的 DOM id（供 Label 关联，读屏与点击标签都能切换）。 */
  switchId: string;
}) {
  return (
    <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
      <p className="text-[13px] font-medium">{title}</p>
      <div className="flex shrink-0 items-center gap-2">
        <Label
          htmlFor={switchId}
          className="cursor-pointer text-xs font-normal text-muted-foreground"
        >
          自动滚动：{autoScroll ? "开" : "关"}
        </Label>
        <Switch
          id={switchId}
          checked={autoScroll}
          onCheckedChange={onAutoScrollChange}
          aria-label="日志自动滚动开关"
        />
      </div>
    </div>
  );
}
