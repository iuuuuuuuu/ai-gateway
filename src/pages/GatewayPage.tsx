import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Link } from "react-router-dom";
import {
  Activity,
  AlertTriangle,
  Bot,
  CheckCircle2,
  Copy,
  Loader2,
  Play,
  RefreshCw,
  RotateCw,
  Save,
  Server,
  Shuffle,
  Square,
  UserRound,
  Wand2,
  Zap,
} from "lucide-react";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Separator } from "@/components/ui/separator";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import type {
  GatewayConfig,
  GatewayMode,
  GatewayPoolAccount,
  GatewayPortCheck,
  GatewayStatus,
  GatewayUsageResult,
} from "@/lib/types";
import { cn } from "@/lib/utils";
import { toast } from "sonner";

interface SectionProps {
  title: string;
  description?: string;
  children: React.ReactNode;
}

function Section({
  title,
  description,
  children,
  className,
}: SectionProps & { className?: string }) {
  return (
    // flex-col + Card 的 flex-1：让同一栅格行内的卡片**等高**。
    // 否则左右两栏内容量不同（左边 4 个状态块、右边 5 行设置）时高度参差，
    // 视觉上像没对齐的拼贴。
    <section className={cn("flex min-w-0 flex-col space-y-2.5", className)}>
      <div className="px-1">
        <h2 className="text-[13px] font-medium leading-5">{title}</h2>
        {description ? (
          <p className="mt-0.5 text-xs text-muted-foreground">{description}</p>
        ) : null}
      </div>
      <Card className="min-w-0 flex-1 gap-0 overflow-hidden rounded-xl py-0 shadow-none">{children}</Card>
    </section>
  );
}

function Row({ children, className }: { children: React.ReactNode; className?: string }) {
  return (
    <div
      className={cn(
        "mx-4 flex min-w-0 flex-wrap items-center justify-between gap-3 border-b border-border/50 py-2.5 last:border-b-0 sm:mx-5",
        className,
      )}
    >
      {children}
    </div>
  );
}

function Stat({
  label,
  value,
  hint,
  tone,
}: {
  label: string;
  value: React.ReactNode;
  hint?: React.ReactNode;
  tone?: "ok" | "warn" | "off";
}) {
  return (
    <div className="min-w-0 rounded-lg border border-border/60 px-3 py-2">
      <div className="text-[11px] text-muted-foreground">{label}</div>
      <div
        className={cn(
          "mt-0.5 truncate text-[15px] font-medium tabular-nums",
          tone === "ok" && "text-emerald-600 dark:text-emerald-400",
          tone === "warn" && "text-amber-600 dark:text-amber-400",
          tone === "off" && "text-muted-foreground",
        )}
      >
        {value}
      </div>
      {hint ? <div className="mt-0.5 truncate text-[11px] tabular-nums text-muted-foreground">{hint}</div> : null}
    </div>
  );
}

/** 网关 Token 用量的统计范围选项。 */
type UsageRangeKey = "today" | "7d" | "30d" | "all";

const USAGE_RANGE_OPTIONS: { key: UsageRangeKey; label: string; days?: number }[] = [
  { key: "today", label: "今日", days: 1 },
  { key: "7d", label: "近 7 天", days: 7 },
  { key: "30d", label: "近 30 天", days: 30 },
  { key: "all", label: "全部" },
];

const exactTokenFormatter = new Intl.NumberFormat("en-US");

/** 大数紧凑展示（与 Token 统计页的 K/M/B 风格一致）。 */
function formatUsageCompact(value: number): string {
  const abs = Math.abs(value);
  if (abs >= 1_000_000_000) return `${(value / 1_000_000_000).toFixed(1)}B`;
  if (abs >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (abs >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return exactTokenFormatter.format(value);
}

/** 一组数值的最大值（至少为 1，避免除零）。 */
function maxOf(values: number[]): number {
  return values.reduce((max, value) => Math.max(max, value), 1);
}

/** 用量分布行：名称 + 占比条 + 数值（可带底部说明）。 */
function UsageBarRow({
  label,
  value,
  max,
  meta,
}: {
  label: string;
  value: number;
  max: number;
  meta?: string;
}) {
  const percent = max > 0 ? Math.max(3, Math.round((value / max) * 100)) : 0;
  return (
    <div className="space-y-1.5 px-4 py-2 sm:px-5">
      <div className="flex items-baseline justify-between gap-3 text-xs">
        <span className="min-w-0 truncate">{label}</span>
        <span className="shrink-0 tabular-nums text-muted-foreground">{formatUsageCompact(value)}</span>
      </div>
      <div className="h-1.5 overflow-hidden rounded-full bg-muted">
        <div className="h-full rounded-full bg-primary/70" style={{ width: `${percent}%` }} />
      </div>
      {meta ? <div className="truncate text-[11px] text-muted-foreground">{meta}</div> : null}
    </div>
  );
}

/** 从监听地址（":7863" / "0.0.0.0:7863"）解析端口。 */
function portOf(listen: string | undefined): number {
  if (!listen) return 0;
  const m = listen.match(/(\d{1,5})\s*$/);
  return m ? Number(m[1]) : 0;
}

/** 端口合法性：1-65535，且不是 1024 以下的特权端口（可用但有提示）。 */
function validatePort(value: number): string | null {
  if (!Number.isInteger(value) || value < 1 || value > 65535) return "端口需在 1-65535 之间";
  return null;
}

/** 把「距今毫秒数」格式化成「刚刚 / 12 秒前 / 3 分钟前」。 */
function formatRelativeTime(deltaMs: number): string {
  const sec = Math.max(0, Math.floor(deltaMs / 1000));
  if (sec < 5) return "刚刚";
  if (sec < 60) return `${sec} 秒前`;
  const min = Math.floor(sec / 60);
  if (min < 60) return `${min} 分钟前`;
  const hour = Math.floor(min / 60);
  if (hour < 24) return `${hour} 小时前`;
  return `${Math.floor(hour / 24)} 天前`;
}

/** 把剩余秒数格式化成「1 小时 5 分钟」这类中文时长。 */
function formatRemaining(sec: number): string {  if (sec <= 0) return "即将恢复";
  const totalMinutes = Math.max(1, Math.ceil(sec / 60));
  const hours = Math.floor(totalMinutes / 60);
  const minutes = totalMinutes % 60;
  if (hours > 0 && minutes > 0) return `${hours} 小时 ${minutes} 分钟`;
  if (hours > 0) return `${hours} 小时`;
  return `${minutes} 分钟`;
}

/** 把冷却截止时刻格式化成「09-15 13:25」（本地时区）。 */
function formatUntil(iso?: string): string | null {
  if (!iso) return null;
  const t = new Date(iso);
  if (Number.isNaN(t.getTime())) return null;
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(t.getMonth() + 1)}-${p(t.getDate())} ${p(t.getHours())}:${p(t.getMinutes())}`;
}

/**
 * 冷却原因说明：区分「账号级（余额欠费）」与「模型级（单一模型限流）」。
 *
 * 这两种状态此前在界面上都只显示"冷却中"，用户无法判断是该充值还是换个模型就好。
 */
function coolReasonText(acc: GatewayPoolAccount): string {
  if (acc.disabled) return acc.reason || "已禁用";
  if (acc.cool_kind === "hard_credit") return "余额不足（积分欠费），等签到或充值后恢复";
  if (acc.cool_kind === "soft_rate") return "账号被限速，短暂冷却后自动恢复";
  if (acc.cool_kind === "breaker") return "连续失败触发熔断，按退避时间恢复";
  return acc.reason || "冷却中";
}

/** 网关账号池账号卡片：展示冷却/熔断/在途等运行态。 */
function PoolAccountRow({ acc }: { acc: GatewayPoolAccount }) {
  const modelCools = acc.model_cooling ?? [];
  // 「冷却中」只表示**账号级**不可用（余额欠费/被限速/熔断）。
  // 模型级限流不影响整号可用性，故单独在下方区域呈现，不占用这个状态标签。
  const state = acc.disabled
    ? { label: "已禁用", cls: "bg-destructive/10 text-destructive" }
    : acc.cooling
      ? { label: "冷却中", cls: "bg-amber-500/10 text-amber-600 dark:text-amber-400" }
      : { label: "健康", cls: "bg-emerald-500/10 text-emerald-600 dark:text-emerald-400" };
  // 到期日就是选号分层档位：同一 expire_day 的账号在均衡时同级（平均分摊）。
  const expiry = acc.expire_day
    ? { label: `到期 ${acc.expire_day.slice(5)}`, title: `最近到期积分：${acc.expire_day}（同一天的账号同级平均分摊）` }
    : { label: "到期未知", title: "尚未取到积分到期信息：会排在其他账号之后，仅在它们不可用时才使用" };
  return (
    // 每个账号是**独立卡片**而非长列表的一行。
    //
    // 原因：此前是无边框的行，靠 border-b 分隔；分两列后在列与列之间没有视觉边界，
    // 且行高随冷却内容参差（有模型冷却的行高一倍），整体看起来像未对齐的拼贴。
    // 独立卡片 + 栅格 auto-rows-fr 后，同排卡片等高、边界清晰。
    <div className="flex min-w-0 flex-col rounded-xl border border-border/60 bg-card/40 p-3">
      <div className="flex min-w-0 items-start gap-3">
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium">{acc.nickname || acc.uid}</div>
          <div className="truncate font-mono text-[11px] text-muted-foreground/80">{acc.uid}</div>
        </div>
        <span
          className={cn("shrink-0 rounded-md px-1.5 py-0.5 text-[11px] font-medium", state.cls)}
          title={coolReasonText(acc)}
        >
          {state.label}
        </span>
      </div>

      {/* 运行数据行：到期档位 + 成功/失败/在途。数值为 0 时不渲染，避免占位抖动。 */}
      <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] tabular-nums text-muted-foreground">
        <span className="text-muted-foreground/70" title={expiry.title}>
          {expiry.label}
        </span>
        {typeof acc.success_count === "number" && acc.success_count > 0 ? (
          <span>成功 {acc.success_count}</span>
        ) : null}
        {typeof acc.err_total === "number" && acc.err_total > 0 ? <span>失败 {acc.err_total}</span> : null}
        {typeof acc.in_flight === "number" && acc.in_flight > 0 ? (
          <span className="text-foreground/80">在途 {acc.in_flight}</span>
        ) : null}
      </div>

      {/* 冷却明细：区分「余额欠费」（账号级）与「模型冷却」（仅单个模型）。
          「模型冷却只影响该模型」这句已上提到区块顶部统一说明，
          不再逐账号重复 —— 14 个账号会重复 14 遍，纯噪音。 */}
      {acc.cooling || modelCools.length > 0 ? (
        <div className="mt-2 flex flex-col gap-1 border-t border-border/50 pt-2 text-[11px]">
          {acc.cooling ? (
            <div className="flex min-w-0 items-center gap-2">
              <span className="shrink-0 rounded bg-amber-500/15 px-1.5 py-0.5 font-medium text-amber-700 dark:text-amber-400">
                {acc.cool_kind === "hard_credit" ? "余额欠费" : acc.cool_kind === "breaker" ? "熔断" : "账号限速"}
              </span>
              <span className="min-w-0 flex-1 truncate text-muted-foreground" title={coolReasonText(acc)}>
                {coolReasonText(acc)}
              </span>
              {typeof acc.cool_remaining_sec === "number" && acc.cool_remaining_sec > 0 ? (
                <span className="shrink-0 tabular-nums text-muted-foreground">
                  剩余 {formatRemaining(acc.cool_remaining_sec)}
                </span>
              ) : null}
            </div>
          ) : null}
          {modelCools.map((mc) => {
            const until = formatUntil(mc.until);
            return (
              <div key={mc.model} className="flex min-w-0 items-center gap-2">
                <span className="shrink-0 rounded bg-sky-500/15 px-1.5 py-0.5 font-medium text-sky-700 dark:text-sky-400">
                  模型冷却
                </span>
                <span className="min-w-0 flex-1 truncate font-mono text-muted-foreground" title={mc.reason || mc.model}>
                  {mc.model}
                </span>
                <span
                  className="shrink-0 tabular-nums text-muted-foreground"
                  title={
                    mc.reset_at_parsed === false
                      ? "上游报错里未给出可解析的重置时间，按固定软冷却时长处理"
                      : until
                        ? `预计 ${until} 恢复`
                        : undefined
                  }
                >
                  {until ? `${until} 恢复` : ""}
                  {typeof mc.remaining_sec === "number" && mc.remaining_sec > 0
                    ? `（剩 ${formatRemaining(mc.remaining_sec)}）`
                    : ""}
                </span>
              </div>
            );
          })}
          {/* 「只影响上述模型」这句全局提示已上提到账号池区块顶部，
              此处不再逐账号重复（14 个账号会重复 14 遍，纯噪音）。 */}
        </div>
      ) : null}
    </div>
  );
}

export default function GatewayPage() {
  const [status, setStatus] = useState<GatewayStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const [port, setPort] = useState(7863);
  const [apiKey, setApiKey] = useState("");
  const [autoStart, setAutoStart] = useState(false);
  /** 网关工作模式：balance 负载均衡 / pinned 指定账号 */
  const [mode, setMode] = useState<GatewayMode>("balance");
  const [pinnedUid, setPinnedUid] = useState<string>("");
  /** 端口可用性检测结果（null = 尚未检测/正在检测）。 */
  const [portCheck, setPortCheck] = useState<GatewayPortCheck | null>(null);
  const [checkingPort, setCheckingPort] = useState(false);

  /** 网关 Token 用量：范围选择、数据与加载态。默认「今日」——看用量多为盯当天消耗。 */
  const [usageRange, setUsageRange] = useState<UsageRangeKey>("today");
  const [usage, setUsage] = useState<GatewayUsageResult | null>(null);
  const [usageLoading, setUsageLoading] = useState(true);
  /** 手动刷新触发的自增序号（同范围下重新拉取）。 */
  const [usageNonce, setUsageNonce] = useState(0);
  /** 用量数据「上次成功更新」的时刻（毫秒）；未成功过则为 null。 */
  const [usageUpdatedAt, setUsageUpdatedAt] = useState<number | null>(null);
  /** 每秒自增，驱动「上次更新 xx 秒前」的相对时间重新渲染。 */
  const [nowTick, setNowTick] = useState(() => Date.now());

  /**
   * 端口 / API Key 是否存在「已编辑但未保存」的内容。
   *
   * 5 秒轮询会用后端配置刷新界面；若无条件覆盖，用户正在输入的内容会被
   * 中途改回去。因此在用户编辑期间暂停对这两个文本字段的覆盖。
   * 模式与自动启动是开关型操作，改为即时保存，不受此影响。
   */
  const dirtyRef = useRef(false);

  const applyConfig = useCallback((cfg: GatewayConfig) => {
    if (!dirtyRef.current) {
      setPort(cfg.port || portOf(cfg.listen) || 7863);
      setApiKey(cfg.api_key || "");
    }
    setAutoStart(Boolean(cfg.auto_start));
    setMode(cfg.mode === "pinned" ? "pinned" : "balance");
    setPinnedUid(cfg.pinned_uid ?? "");
  }, []);

  /**
   * 切换工作模式并立即生效。
   *
   * 两件事必须一起做，否则用户看到的是「点了没反应」：
   *  1. 模式属于开关型设置：若只改本地状态而等用户点「保存」，5 秒后的轮询会用
   *     后端旧值把它覆盖回负载均衡（用户看到的「点了一会又跳回去」）。
   *  2. 网关账号池是**启动时**扫描凭证目录建立的，光写配置不会改变池内容，
   *     因此必须重导出凭证并重启网关才真正生效。
   * `switchGatewayMode` 在 core 里把「保存 + 重导出 + 按需重启」合成一步。
   */
  async function changeMode(next: GatewayMode) {
    const uid =
      next === "pinned"
        ? pinnedUid || status?.accounts?.[0]?.uid || ""
        : null;
    if (next === "pinned" && !uid) {
      toast.error("「指定账号」模式需要先选择一个账号");
      return;
    }
    setMode(next);
    setPinnedUid(uid ?? "");
    try {
      const res = await api.switchGatewayMode(next, uid);
      if (res.reloaded) {
        toast.success(next === "pinned" ? "已切换为指定账号并重启网关" : "已切换为负载均衡并重启网关");
      }
      await refresh();
    } catch (e) {
      toast.error(api.asError(e));
      await refresh(); // 回滚为后端真实状态
    }
  }

  /**
   * 切换「随 App 启动」。同样是开关型设置，立即持久化，
   * 否则 5 秒轮询会用后端旧值拨回开关。
   */
  async function changeAutoStart(next: boolean) {
    setAutoStart(next);
    try {
      await api.saveGatewayConfig({ auto_start: next });
      await refresh();
    } catch (e) {
      toast.error(api.asError(e));
      await refresh();
    }
  }

  /** 指定账号模式下切换目标账号，同样立即生效（重导出凭证 + 按需重启）。 */
  async function changePinnedUid(uid: string) {
    setPinnedUid(uid);
    try {
      await api.switchGatewayMode("pinned", uid);
      await refresh();
    } catch (e) {
      toast.error(api.asError(e));
      await refresh();
    }
  }

  const refresh = useCallback(async () => {
    try {
      const s = await api.getGatewayStatus();
      setStatus(s);
      applyConfig(s.config);
      setError(null);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, [applyConfig]);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void refresh(), 5000);
    return () => window.clearInterval(timer);
  }, [refresh]);

  // Token 用量按所选范围拉取。
  //
  // 自动刷新：用量区块需要周期更新（网关在持续接请求），但**不**适合挤进 status 的
  // 5 秒轮询 —— 聚合响应可能较大。这里用独立的 30 秒周期，既能自动跟进，
  // 又不会让「切页即请求」把开销放大。
  useEffect(() => {
    let cancelled = false;
    setUsageLoading(true);
    const days = USAGE_RANGE_OPTIONS.find((option) => option.key === usageRange)?.days;
    api
      .getGatewayUsage(days)
      .then((res) => {
        if (!cancelled) {
          setUsage(res);
          // 只在拿到有效快照时更新"上次更新"，失败不该刷新这个时间戳
          // （否则界面会显示一个刚更新过、但其实是错误结果的时刻）。
          if (res.usage) setUsageUpdatedAt(Date.now());
        }
      })
      .catch((e) => {
        if (!cancelled) {
          setUsage({ running: false, reachable: false, usage: null, error: api.asError(e) });
        }
      })
      .finally(() => {
        if (!cancelled) setUsageLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [usageRange, usageNonce]);

  // 用量自动刷新（30 秒一轮）。
  //
  // 只在网关确实在跑时才轮询：未启动时数据不会变，白打请求。
  // 这里直接读 status?.running 而不用下面的 `running` 变量 —— 后者在组件更下方
  // 才声明，在此处引用会命中 TDZ。
  // usageNonce 变化（手动刷新）会重置定时器，避免刚手动刷完又立刻自动刷一次。
  useEffect(() => {
    if (!status?.running) return;
    const timer = window.setInterval(() => setUsageNonce((n) => n + 1), 30_000);
    return () => window.clearInterval(timer);
  }, [status?.running, usageRange, usageNonce]);

  // 每秒 tick 一次，只为让「上次更新 x 秒前」这类相对时间保持新鲜。
  // 与用量请求解耦：不额外发请求，仅触发一次廉价的重渲染。
  useEffect(() => {
    const timer = window.setInterval(() => setNowTick(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);

  // 端口变化后防抖检测可用性。
  // 网关正跑在自己的端口上时该端口必然「被占用」，此时不报冲突。
  useEffect(() => {
    const invalid = validatePort(port);
    if (invalid) {
      setPortCheck(null);
      return;
    }
    let cancelled = false;
    setCheckingPort(true);
    const timer = window.setTimeout(async () => {
      try {
        const res = await api.checkGatewayPort(port);
        if (!cancelled) setPortCheck(res);
      } catch {
        if (!cancelled) setPortCheck(null);
      } finally {
        if (!cancelled) setCheckingPort(false);
      }
    }, 350);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
      setCheckingPort(false);
    };
  }, [port, status?.running]);

  /** 自动挑一个空闲端口。 */
  async function pickFreePort() {
    setCheckingPort(true);
    try {
      // 从当前端口往后找；当前端口自身被网关占用时也能跳过
      for (let candidate = Math.max(port, 1024); candidate < port + 60; candidate += 1) {
        const res = await api.checkGatewayPort(candidate);
        if (res.available || res.inUseByGateway) {
          setPort(candidate);
          setPortCheck(res);
          toast.success(`已选择端口 ${candidate}`);
          return;
        }
      }
      toast.error("未找到空闲端口，请手动指定");
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setCheckingPort(false);
    }
  }

  /** 端口状态文案与配色。 */
  const portState = (() => {
    const invalid = validatePort(port);
    if (invalid) return { label: invalid, tone: "bad" as const };
    if (checkingPort || !portCheck) return { label: "检测中…", tone: "muted" as const };
    // 网关自己正跑在该端口上时，端口「被占用」是正常的
    if (portCheck.inUseByGateway) return { label: "当前网关正在使用", tone: "ok" as const };
    if (portCheck.available) {
      return portCheck.reserved
        ? { label: "可用（特权端口，可能需管理员权限）", tone: "warn" as const }
        : { label: "可用", tone: "ok" as const };
    }
    return {
      label: portCheck.suggest ? `已被占用，建议改用 ${portCheck.suggest}` : "已被占用",
      tone: "bad" as const,
    };
  })();

  async function run(label: string, fn: () => Promise<unknown>) {
    setBusy(label);
    try {
      await fn();
      await refresh();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(null);
    }
  }

  const pool = status?.pool ?? null;
  const poolAccounts = pool?.accounts ?? [];
  /** 处于模型冷却的账号数（用于区块顶部的统一说明，替代逐账号重复提示）。 */
  const poolModelCooledCount = poolAccounts.filter((a) => (a.model_cooling?.length ?? 0) > 0).length;
  const running = Boolean(status?.running);
  /** 因需重新登录被排除出账号池的账号（后端同步时不导出其凭证）。 */
  const excludedAccounts = status?.excludedAccounts ?? [];

  // 用量区块的派生数据。
  const usageSnapshot = usage?.usage ?? null;
  const usageSummary = usageSnapshot?.summary ?? null;
  const usageModels = usageSnapshot?.models ?? [];
  const usageAccounts = usageSnapshot?.accounts ?? [];
  const usageDaily = usageSnapshot?.daily ?? [];
  const usageMaxModel = maxOf(usageModels.map((m) => m.total));
  const usageMaxAccount = maxOf(usageAccounts.map((a) => a.total));
  const usageMaxDaily = maxOf(usageDaily.map((d) => d.total));
  const usageNickname = useMemo(() => {
    const map = new Map<string, string>();
    for (const account of status?.accounts ?? []) {
      map.set(account.uid, account.nickname || account.uid.slice(0, 8));
    }
    return map;
  }, [status?.accounts]);

  const endpoint = status?.openaiBase ?? "";
  const endpointHint = useMemo(() => {
    if (!endpoint) return "";
    return `OPENAI_BASE_URL=${endpoint}`;
  }, [endpoint]);

  async function copyEndpoint() {
    if (!endpoint) return;
    try {
      await navigator.clipboard.writeText(endpoint);
      toast.success("已复制接口地址");
    } catch {
      toast.error("复制失败，请手动选择文本");
    }
  }

  return (
    // 流式布局：内容随窗口铺满（减去侧栏），只保留内边距。
    //
    // 留白的演进（按 2340px 屏、侧栏 220px 计算，每侧留白）：
    //   max-w-3xl (768px)    → 676px
    //   max-w-[1180px]       → 470px   ← 用户反馈"还是很多留白"
    //   去除上限（本版）      → 0（仅 px-8 内边距）
    // 结论：只要保留居中定宽，宽屏上必然有大片留白；网关页的内容
    //（状态块、设置行、账号池卡片、用量表）本身都适合变宽，故直接放开。
    // 仍保留 max-w-[1800px] 作为超宽屏（4K/带鱼屏）兜底，避免单行文字过长难扫读。
    <div className="mx-auto w-full max-w-[1800px] space-y-6 px-5 py-6 sm:px-8 sm:py-8">
      <header className="flex min-w-0 items-start justify-between gap-3">
        <div className="min-w-0">
          <h1 className="flex items-center gap-2 text-lg font-medium leading-6">
            <Server className="size-4.5 shrink-0" />
            兼容网关
          </h1>
          <p className="mt-1 text-xs text-muted-foreground">
            把账号库里的账号变成 OpenAI 兼容接口，供任意 SDK / 客户端使用。
          </p>
        </div>
        <Button
          variant="ghost"
          size="icon"
          className="shrink-0"
          onClick={() => void refresh()}
          aria-label="刷新"
          disabled={busy !== null}
        >
          <RefreshCw className={cn("size-4", busy === "refresh" && "animate-spin")} />
        </Button>
      </header>

      {error ? (
        <Alert variant="destructive">
          <AlertTriangle />
          <AlertTitle>无法读取网关状态</AlertTitle>
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      ) : null}

      {status && !status.exeFound ? (
        <Alert>
          <AlertTriangle />
          <AlertTitle>未找到网关可执行文件</AlertTitle>
          <AlertDescription>
            请把 <code className="font-mono">gateway.exe</code> 放到 workbuddy-switch
            同目录，或用环境变量 <code className="font-mono">WB_SWITCH_GATEWAY_BIN</code> 指定路径。
          </AlertDescription>
        </Alert>
      ) : null}

      {/* 运行状态与接口配置并排：两者都是窄内容（状态块 + 若干设置行），
          在宽屏上各占一列比上下堆叠更省纵向空间，也把横向空间用起来。 */}
      <div className="grid min-w-0 gap-6 xl:grid-cols-2">
        <Section title="运行状态" description="账号池状态每 5 秒自动刷新">
        {excludedAccounts.length > 0 && (
          <div className="mx-4 mt-3 rounded-lg border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-900 sm:mx-5">
            <div className="flex items-center gap-1.5 font-medium">
              <AlertTriangle className="size-3.5" />
              {excludedAccounts.length} 个账号需重新登录，已排除出账号池
            </div>
            <div className="mt-1 leading-5 opacity-90">
              {excludedAccounts.map((a) => a.nickname || a.uid).join("、")}
              ：refresh token 已失效，继续使用只会让每次请求失败一次。请到「账号管理」页重新登录，
              恢复后会自动重新加入账号池。
            </div>
          </div>
        )}
        <div className="mx-4 grid grid-cols-2 gap-2 py-3 sm:mx-5 sm:grid-cols-4">
          <Stat
            label="服务"
            value={loading ? "…" : running ? (status?.reachable ? "运行中" : "已启动") : "未运行"}
            tone={running ? "ok" : "off"}
          />
          <Stat label="健康账号" value={pool?.healthy ?? "—"} tone={(pool?.healthy ?? 0) > 0 ? "ok" : "warn"} />
          <Stat label="冷却 / 禁用" value={`${pool?.cooling ?? 0} / ${pool?.disabled ?? 0}`} tone="warn" />
          <Stat label="粘性会话" value={pool?.sticky_sessions ?? 0} />
        </div>

        <Row>
          <div className="min-w-0">
            <div className="text-[13px]">OpenAI 兼容接口</div>
            <div className="mt-0.5 truncate font-mono text-[11px] text-muted-foreground">
              {endpoint || "—"}
            </div>
          </div>
          <div className="flex shrink-0 items-center gap-1.5">
            <Button
              variant="ghost"
              size="icon"
              className="size-7"
              onClick={() => void copyEndpoint()}
              disabled={!endpoint}
              aria-label="复制接口地址"
            >
              <Copy className="size-3.5" />
            </Button>
            {running ? (
              <>
                <Button
                  variant="outline"
                  size="sm"
                  className="h-7 gap-1.5 text-xs"
                  onClick={() => void run("restart", () => api.restartGateway())}
                  disabled={busy !== null}
                >
                  {busy === "restart" ? <Loader2 className="size-3.5 animate-spin" /> : <RotateCw className="size-3.5" />}
                  重启
                </Button>
                <Button
                  variant="outline"
                  size="sm"
                  className="h-7 gap-1.5 text-xs"
                  onClick={() => void run("stop", () => api.stopGateway())}
                  disabled={busy !== null}
                >
                  {busy === "stop" ? <Loader2 className="size-3.5 animate-spin" /> : <Square className="size-3.5" />}
                  停止
                </Button>
              </>
            ) : (
              <Button
                size="sm"
                className="h-7 gap-1.5 text-xs"
                onClick={() => void run("start", () => api.startGateway(port))}
                disabled={busy !== null || !status?.exeFound || portState.tone === "bad"}
              >
                {busy === "start" ? <Loader2 className="size-3.5 animate-spin" /> : <Play className="size-3.5" />}
                启动网关
              </Button>
            )}
          </div>
        </Row>

        <Row>
          <div className="min-w-0">
            <div className="text-[13px]">账号同步</div>
            <div className="mt-0.5 text-[11px] text-muted-foreground">
              账号库 {status?.accountsInLibrary ?? "—"} 个账号 · 变更会自动同步
              {running ? "，网关运行中会自动重启以加载新账号" : ""}
            </div>
          </div>
          <Button
            variant="outline"
            size="sm"
            className="h-7 shrink-0 gap-1.5 text-xs"
            onClick={() => void run("sync", () => api.syncGatewayAccounts(true))}
            disabled={busy !== null}
          >
            {busy === "sync" ? <Loader2 className="size-3.5 animate-spin" /> : <Zap className="size-3.5" />}
            立即同步
          </Button>
        </Row>
        </Section>

        <Section title="接口配置">
        {/* 工作模式 */}
        <Row className="flex-col items-stretch gap-2 sm:flex-row sm:items-center">
          <div className="min-w-0">
            <div className="text-[13px]">工作模式</div>
            <div className="mt-0.5 text-[11px] text-muted-foreground">
              {mode === "balance"
                ? "先打最近到期的积分，同一天到期的账号平均分摊（点击即时生效）"
                : "只使用下方指定的这一个账号（点击即时生效）"}
            </div>
          </div>
          <div className="flex shrink-0 items-center gap-1.5">
            <Button
              variant={mode === "balance" ? "default" : "outline"}
              size="sm"
              className="h-8 gap-1.5 px-2.5 text-xs"
              onClick={() => void changeMode("balance")}
            >
              <Shuffle className="size-3.5" />
              负载均衡
            </Button>
            <Button
              variant={mode === "pinned" ? "default" : "outline"}
              size="sm"
              className="h-8 gap-1.5 px-2.5 text-xs"
              onClick={() => void changeMode("pinned")}
            >
              <UserRound className="size-3.5" />
              指定账号
            </Button>
          </div>
        </Row>

        {/* 指定账号模式下选择账号 */}
        {mode === "pinned" ? (
          <Row className="flex-col items-stretch gap-2 sm:flex-row sm:items-center">
            <div className="min-w-0">
              <Label htmlFor="gw-account" className="text-[13px] font-normal">
                使用账号
              </Label>
              <div className="mt-0.5 text-[11px] text-muted-foreground">
                {status?.accounts?.length
                  ? `共 ${status.accounts.length} 个账号可选`
                  : "账号库为空"}
              </div>
            </div>
            <select
              id="gw-account"
              value={pinnedUid}
              onChange={(e) => void changePinnedUid(e.target.value)}
              className="h-8 w-44 shrink-0 rounded-md border border-input bg-background px-2 text-xs"
            >
              <option value="">（未选择）</option>
              {(status?.accounts ?? []).map((a) => (
                <option key={a.uid} value={a.uid}>
                  {a.nickname || a.uid.slice(0, 8)}
                  {a.needsRelogin ? "（需重新登录）" : ""}
                </option>
              ))}
            </select>
          </Row>
        ) : null}

        <Row className="flex-col items-stretch gap-2 sm:flex-row sm:items-center">
          <div className="min-w-0">
            <Label htmlFor="gw-port" className="text-[13px] font-normal">
              服务端口
            </Label>
            <div className="mt-0.5 flex items-center gap-1.5 text-[11px]">
              <span
                className={cn(
                  "size-1.5 shrink-0 rounded-full",
                  portState.tone === "ok" && "bg-emerald-500",
                  portState.tone === "warn" && "bg-amber-500",
                  portState.tone === "bad" && "bg-destructive",
                  portState.tone === "muted" && "bg-muted-foreground/40",
                )}
                aria-hidden="true"
              />
              <span
                className={cn(
                  "text-muted-foreground",
                  portState.tone === "bad" && "text-destructive",
                  portState.tone === "warn" && "text-amber-600 dark:text-amber-400",
                )}
              >
                {portState.label}
              </span>
            </div>
          </div>
          <div className="flex shrink-0 items-center gap-1.5">
            <Input
              id="gw-port"
              type="number"
              min={1}
              max={65535}
              value={Number.isFinite(port) ? port : ""}
              onChange={(e) => {
                dirtyRef.current = true;
                setPort(Number(e.target.value));
              }}
              placeholder="7863"
              className="h-8 w-24 font-mono text-xs"
              aria-invalid={portState.tone === "bad"}
            />
            <Button
              variant="outline"
              size="sm"
              className="h-8 gap-1.5 px-2 text-xs"
              onClick={() => void pickFreePort()}
              disabled={checkingPort}
              title="自动挑一个空闲端口"
            >
              {checkingPort ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <Wand2 className="size-3.5" />
              )}
              自动
            </Button>
            {portCheck?.suggest && !portCheck.available ? (
              <Button
                variant="outline"
                size="sm"
                className="h-8 px-2 text-xs"
                onClick={() => setPort(portCheck.suggest as number)}
              >
                用 {portCheck.suggest}
              </Button>
            ) : null}
          </div>
        </Row>
        <Row>
          <div className="min-w-0">
            <Label htmlFor="gw-key" className="text-[13px] font-normal">
              API Key
            </Label>
            <div className="mt-0.5 text-[11px] text-muted-foreground">留空不鉴权；公网部署务必设置</div>
          </div>
          <Input
            id="gw-key"
            value={apiKey}
            onChange={(e) => {
              dirtyRef.current = true;
              setApiKey(e.target.value);
            }}
            placeholder="sk-..."
            className="h-8 w-44 shrink-0 font-mono text-xs"
          />
        </Row>
        <Row>
          <div className="min-w-0">
            <div className="text-[13px]">随 App 启动</div>
            <div className="mt-0.5 text-[11px] text-muted-foreground">打开本应用时自动启动网关</div>
          </div>
          <div className="flex shrink-0 items-center gap-2">
            <Switch checked={autoStart} onCheckedChange={(v) => void changeAutoStart(v)} />
            <Button
              variant="outline"
              size="sm"
              className="h-7 gap-1.5 text-xs"
              onClick={() =>
                // 保存按钮只管文本字段（端口 / API Key）：
                // 模式与自动启动是开关型，已在点击时即时保存，不在这里重复提交。
                void run("save", async () => {
                  dirtyRef.current = false;
                  await api.saveGatewayConfig({ port, api_key: apiKey });
                  toast.success("配置已保存");
                })
              }
              disabled={busy !== null}
            >
              {busy === "save" ? <Loader2 className="size-3.5 animate-spin" /> : <Save className="size-3.5" />}
              保存
            </Button>
          </div>
        </Row>
        </Section>
      </div>

      <Section title="账号池" description={pool ? `网关侧运行态（redis=${pool.redis_mode ?? "noop"}）` : "启动网关后可见"}>
        {poolAccounts.length > 0 ? (
          <>
            {/* 全局说明只写一次。此前每个账号都重复渲染「模型冷却只影响上述模型…」，
                14 个账号就是 14 遍相同文案，是纯噪音。 */}
            {poolModelCooledCount > 0 ? (
              <div className="mx-4 mt-3 rounded-lg bg-sky-500/10 px-3 py-2 text-[11px] leading-5 text-sky-800 dark:text-sky-300 sm:mx-5">
                有 {poolModelCooledCount} 个账号处于<strong className="font-medium">模型冷却</strong>
                ：仅下列标出的模型暂不可用，这些账号的<strong className="font-medium">其他模型仍会正常参与负载均衡</strong>，
                冷却到期后自动恢复。
              </div>
            ) : null}
            {/* 卡片网格：auto-rows-fr 让同一排的卡片等高，避免因冷却明细行数不同而参差。 */}
            <div className="grid auto-rows-fr grid-cols-1 gap-3 p-3 sm:grid-cols-2 sm:p-4 xl:grid-cols-3 2xl:grid-cols-4">
              {poolAccounts.map((acc) => <PoolAccountRow key={acc.uid} acc={acc} />)}
            </div>
          </>
        ) : (
          <Row className="justify-center">
            <div className="flex items-center gap-2 py-4 text-xs text-muted-foreground">
              {running ? (
                <>
                  <Activity className="size-3.5" />
                  账号池为空，请先同步账号并重启网关
                </>
              ) : (
                <>
                  <CheckCircle2 className="size-3.5" />
                  网关未运行
                </>
              )}
            </div>
          </Row>
        )}
      </Section>

      <Section
        title="Token 用量"
        description="经网关成功请求的上游用量，按模型 / 账号 / 日期聚合（网关重启后保留）"
      >
        <Row>
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
              <span className="text-[13px]">统计范围</span>
              {/* 上次更新时间：让用户知道看到的数据有多新，不必反复手点刷新。
                  相对时间由每秒 tick 驱动；悬停可见精确时刻。 */}
              {usageUpdatedAt ? (
                <span
                  className="text-[11px] tabular-nums text-muted-foreground"
                  title={`上次更新：${new Date(usageUpdatedAt).toLocaleString("zh-CN")}`}
                >
                  上次更新 {formatRelativeTime(nowTick - usageUpdatedAt)}
                  {usageLoading ? " · 更新中…" : ""}
                </span>
              ) : null}
            </div>
            <div className="mt-0.5 text-[11px] tabular-nums text-muted-foreground">
              {usageSummary
                ? `共 ${exactTokenFormatter.format(usageSummary.records)} 次调用 · 合计 ${exactTokenFormatter.format(usageSummary.total)} tokens`
                : "等待网关数据"}
            </div>
          </div>
          <div className="flex shrink-0 items-center gap-1.5">
            {USAGE_RANGE_OPTIONS.map((option) => (
              <Button
                key={option.key}
                variant={usageRange === option.key ? "default" : "outline"}
                size="sm"
                className="h-7 px-2 text-xs"
                onClick={() => setUsageRange(option.key)}
              >
                {option.label}
              </Button>
            ))}
            <Button
              variant="ghost"
              size="icon"
              className="size-7"
              onClick={() => setUsageNonce((value) => value + 1)}
              disabled={usageLoading}
              aria-label="刷新用量"
            >
              <RefreshCw className={cn("size-3.5", usageLoading && "animate-spin")} />
            </Button>
          </div>
        </Row>

        {usageLoading && !usageSnapshot ? (
          <Row className="justify-center">
            <div className="flex items-center gap-2 py-4 text-xs text-muted-foreground">
              <Loader2 className="size-3.5 animate-spin" />
              正在读取网关用量…
            </div>
          </Row>
        ) : (usage && !usage.running) || !running ? (
          <Row className="justify-center">
            <div className="flex items-center gap-2 py-4 text-xs text-muted-foreground">
              <CheckCircle2 className="size-3.5" />
              网关未运行，启动后这里会展示经网关请求的 Token 用量
            </div>
          </Row>
        ) : !usage?.reachable ? (
          <Row className="justify-center">
            <div className="flex items-center gap-2 py-4 text-xs text-muted-foreground">
              <Activity className="size-3.5" />
              网关已启动但暂时无法读取用量{usage?.error ? `：${usage.error}` : ""}
            </div>
          </Row>
        ) : usageSnapshot?.enabled === false ? (
          <Row className="justify-center">
            <div className="flex items-center gap-2 py-4 text-xs text-muted-foreground">
              <AlertTriangle className="size-3.5" />
              当前网关可执行文件不支持用量统计，请更新网关后重试
            </div>
          </Row>
        ) : usageSnapshot && usageSummary ? (
          <>
            <div className="mx-4 grid grid-cols-2 gap-2 py-3 sm:mx-5 sm:grid-cols-4">
              <Stat
                label="总 Token"
                value={formatUsageCompact(usageSummary.total)}
                hint={exactTokenFormatter.format(usageSummary.total)}
              />
              <Stat
                label="输入"
                value={formatUsageCompact(usageSummary.input)}
                hint={
                  usageSummary.cacheHitRate != null
                    ? `缓存命中率 ${(usageSummary.cacheHitRate * 100).toFixed(1)}%`
                    : "无缓存读取数据"
                }
              />
              <Stat
                label="输出"
                value={formatUsageCompact(usageSummary.output)}
                hint={`缓存写入 ${formatUsageCompact(usageSummary.cacheWrite)}`}
              />
              <Stat
                label="调用次数"
                value={exactTokenFormatter.format(usageSummary.records)}
                hint="成功请求"
              />
            </div>

            <div className="grid gap-4 border-t border-border/50 pb-2 pt-3 sm:grid-cols-2">
              <div className="min-w-0">
                <div className="px-4 text-[12px] font-medium text-muted-foreground sm:px-5">
                  按模型
                  {usageModels.length > 0 ? (
                    <span className="ml-1.5 font-normal text-muted-foreground/70">
                      共 {usageModels.length} 个
                    </span>
                  ) : null}
                </div>
                <div className="mt-1 max-h-72 overflow-y-auto">
                  {usageModels.length > 0 ? (
                    usageModels.map((model) => (
                      <UsageBarRow
                        key={model.key}
                        label={model.key}
                        value={model.total}
                        max={usageMaxModel}
                        meta={`${exactTokenFormatter.format(model.records)} 次调用 · 输入 ${formatUsageCompact(model.input)} / 输出 ${formatUsageCompact(model.output)}`}
                      />
                    ))
                  ) : (
                    <div className="px-4 py-2 text-xs text-muted-foreground sm:px-5">该范围内暂无数据</div>
                  )}
                </div>
              </div>
              <div className="min-w-0">
                <div className="px-4 text-[12px] font-medium text-muted-foreground sm:px-5">
                  按账号
                  {usageAccounts.length > 0 ? (
                    <span className="ml-1.5 font-normal text-muted-foreground/70">
                      共 {usageAccounts.length} 个
                    </span>
                  ) : null}
                </div>
                {/*
                  这里**不能**截断成前 5 个：账号池的均衡效果正是靠这个列表观察的。
                  原先写死 slice(0, 5)，导致 8 个账号都在正常轮转、界面却只显示 5 个，
                  用户据此误判「负载均衡只用到 5 个账号」。
                  改为全量展示并加滚动上限（高度受限，避免账号多时把页面撑得过长）。
                */}
                <div className="mt-1 max-h-72 overflow-y-auto">
                  {usageAccounts.length > 0 ? (
                    usageAccounts.map((account) => (
                      <UsageBarRow
                        key={account.key}
                        label={usageNickname.get(account.key) ?? `${account.key.slice(0, 8)}…`}
                        value={account.total}
                        max={usageMaxAccount}
                        meta={`${exactTokenFormatter.format(account.records)} 次调用 · ${account.key.slice(0, 8)}`}
                      />
                    ))
                  ) : (
                    <div className="px-4 py-2 text-xs text-muted-foreground sm:px-5">该范围内暂无数据</div>
                  )}
                </div>
              </div>
            </div>

            {usageDaily.length > 0 ? (
              <div className="border-t border-border/50 px-4 pb-3 pt-3 sm:px-5">
                <div className="text-[12px] font-medium text-muted-foreground">每日用量</div>
                <div className="mt-2 flex h-16 items-end gap-1">
                  {usageDaily.slice(-30).map((day) => (
                    <div
                      key={day.key}
                      className="flex h-full flex-1 items-end"
                      title={`${day.key} · ${exactTokenFormatter.format(day.total)} tokens · ${day.records} 次调用`}
                    >
                      <div
                        className="w-full rounded-t-[3px] bg-primary/60 transition-colors hover:bg-primary"
                        style={{ height: `${Math.max(4, Math.round((day.total / usageMaxDaily) * 100))}%` }}
                      />
                    </div>
                  ))}
                </div>
                <div className="mt-1 flex justify-between text-[10px] text-muted-foreground">
                  <span>{usageDaily[Math.max(0, usageDaily.length - 30)]?.key}</span>
                  <span>{usageDaily[usageDaily.length - 1]?.key}</span>
                </div>
              </div>
            ) : null}
          </>
        ) : (
          <Row className="justify-center">
            <div className="flex items-center gap-2 py-4 text-xs text-muted-foreground">
              <AlertTriangle className="size-3.5" />
              无法读取网关用量{usage?.error ? `：${usage.error}` : ""}
            </div>
          </Row>
        )}
      </Section>

      <Section title="客户端接入" description="把网关接入本机已安装的 AI 客户端，或按标准环境变量接入">
        <div className="space-y-4 p-4 sm:p-5">
          <div className="flex flex-wrap items-center justify-between gap-2.5 rounded-xl border border-primary/30 bg-primary/5 p-3 text-xs">
            <div className="flex items-center gap-2">
              <Bot className="size-4 text-primary shrink-0" />
              <span>现已提供独立的「智能体管理」页面，支持 11 类智能体的多模型多选与一键批量更新。</span>
            </div>
            <Button size="sm" variant="outline" className="h-7 text-xs font-medium" asChild>
              <Link to="/agents">前往智能体管理 →</Link>
            </Button>
          </div>

          <Separator className="my-1" />

          <div>
            <h3 className="text-[13px] font-medium leading-5">手动环境变量配置</h3>
            <p className="mt-0.5 text-xs text-muted-foreground">
              如需在其他第三方工具、SDK 或自建服务中使用网关，可设置以下环境变量：
            </p>
            <div className="mt-2.5 space-y-2.5">
              <div>
                <div className="mb-1 text-[11px] font-medium text-muted-foreground">
                  OpenAI 兼容接口（/v1/chat/completions 与 /v1/models）
                </div>
                <pre className="overflow-x-auto rounded-lg bg-muted/50 px-3 py-2 text-[11px] leading-relaxed">
                  <code>{endpointHint || "OPENAI_BASE_URL=http://127.0.0.1:7863/v1"}
{`OPENAI_API_KEY=${apiKey || "<你的 api_key>"}`}</code>
                </pre>
              </div>
              <div>
                <div className="mb-1 text-[11px] font-medium text-muted-foreground">
                  Anthropic Messages 接口（Claude Code 与 Claude Desktop，/v1/messages）
                </div>
                <pre className="overflow-x-auto rounded-lg bg-muted/50 px-3 py-2 text-[11px] leading-relaxed">
                  <code>{`ANTHROPIC_BASE_URL=http://127.0.0.1:${port || 7863}
ANTHROPIC_AUTH_TOKEN=${apiKey || "<你的 api_key>"}`}</code>
                </pre>
              </div>
            </div>
            <p className="mt-2 text-[11px] text-muted-foreground">
              同时支持 <code className="font-mono">POST /v1/responses</code>（兼容新版 Codex CLI 0.146+），现有主流 AI 客户端均可零改造对接。
            </p>
          </div>
        </div>
      </Section>

      <Section title="诊断">
        <Row>
          <div className="min-w-0">
            <div className="text-[13px]">网关账号凭证目录</div>
            <div className="mt-0.5 break-all font-mono text-[11px] text-muted-foreground">
              {status?.authDir || "—"}
            </div>
          </div>
        </Row>
        <Row>
          <div className="min-w-0">
            <div className="flex items-center gap-2 text-[13px]">
              网关可执行文件
              <Badge variant="secondary" className="text-[10px]">
                {status?.exeSource === "embedded"
                  ? "内嵌"
                  : status?.exeSource === "env"
                    ? "环境变量"
                    : "外部文件"}
              </Badge>
            </div>
            <div className="mt-0.5 break-all font-mono text-[11px] text-muted-foreground">
              {status?.exePath || "未找到"}
            </div>
            {status?.exeSource === "embedded" ? (
              <div className="mt-0.5 text-[11px] text-muted-foreground">
                随主程序分发，首次使用自动释放到本机缓存
              </div>
            ) : null}
          </div>
          <Badge variant={status?.exeFound ? "secondary" : "destructive"} className="shrink-0 text-[10px]">
            {status?.exeFound ? "已就绪" : "缺失"}
          </Badge>
        </Row>
      </Section>
    </div>
  );
}
