import { useCallback, useEffect, useState } from "react";
import { CalendarCheck, Loader2, TrendingUp } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Skeleton } from "@/components/ui/skeleton";
import * as api from "@/lib/api";
import { cn } from "@/lib/utils";

/** 区间选择。上限 90 天与后端保留期一致。 */
const RANGES = [
  { days: 14, label: "近 14 天" },
  { days: 30, label: "近 30 天" },
  { days: 90, label: "近 90 天" },
];

/**
 * 签到成功率趋势（堆叠柱）。
 *
 * 只画**有记录**的日期：补齐缺失的天会画出一根 0 高度的柱子，
 * 看起来像「那天全军覆没」，实际是那天根本没跑签到。
 */
export function CheckinTrends({ refreshKey }: { refreshKey: number }) {
  const [data, setData] = useState<api.TraeCheckinTrends | null>(null);
  const [days, setDays] = useState(30);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setData(await api.traeCheckinTrends(days));
    } catch {
      // 趋势是附加信息，取不到就不显示，不打断主流程
      setData(null);
    } finally {
      setLoading(false);
    }
  }, [days]);

  useEffect(() => {
    void load();
  }, [load, refreshKey]);

  if (loading) return <Skeleton className="h-24 w-full" />;

  const points = data?.points ?? [];
  // 后端已保证不产出 total=0 的日期，但这里仍按 p.total>0 过滤一遍：
  // 旧版本写下的历史数据可能带空条目，除零会渲染出 NaN% 宽度把柱状图搞乱。
  const drawable = points.filter((p) => p.total > 0);
  const s = data?.summary;

  if (drawable.length === 0) {
    return (
      <div className="flex items-center gap-2 py-3 text-xs text-muted-foreground">
        <TrendingUp className="size-3.5" />
        还没有签到记录。跑一次签到后这里会开始累计成功率趋势。
      </div>
    );
  }

  // 柱高按当日总数归一：同一天里成功 / 已签 / 失败按比例分段
  const maxTotal = Math.max(1, ...drawable.map((p) => p.total));
  const rate = s?.successRate;

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2 text-xs">
          <TrendingUp className="size-3.5 text-muted-foreground" />
          <span className="font-medium">签到趋势</span>
          {rate !== null && rate !== undefined && (
            <Badge
              variant="outline"
              className={cn(
                "text-[10px]",
                rate >= 95
                  ? "text-emerald-600 dark:text-emerald-400"
                  : rate >= 80
                    ? "text-amber-600 dark:text-amber-400"
                    : "text-destructive",
              )}
            >
              成功率 {rate.toFixed(0)}%
            </Badge>
          )}
          {s && (
            <span className="text-muted-foreground">
              {s.observedDays} 天 · 成功 {s.ok} · 已签 {s.already}
              {s.failed > 0 && <span className="text-destructive"> · 失败 {s.failed}</span>}
            </span>
          )}
        </div>
        <div className="flex gap-1 rounded-lg bg-muted p-0.5" role="tablist" aria-label="签到统计区间">
          {RANGES.map((r) => (
            <button
              key={r.days}
              type="button"
              role="tab"
              aria-selected={days === r.days}
              onClick={() => setDays(r.days)}
              className={cn(
                "rounded-md px-2 py-0.5 text-[11px] transition-colors",
                days === r.days
                  ? "bg-background text-foreground shadow-sm"
                  : "text-muted-foreground hover:text-foreground",
              )}
            >
              {r.label}
            </button>
          ))}
        </div>
      </div>

      <div className="flex h-20 items-end gap-1 overflow-x-auto pb-1">
        {drawable.map((p) => {
          const h = (p.total / maxTotal) * 100;
          // 用 flex 比例分段而不是绝对高度：总数不同的两天柱高才可比
          const okPct = (p.ok / p.total) * 100;
          const alreadyPct = (p.already / p.total) * 100;
          const failPct = (p.failed / p.total) * 100;
          return (
            <div
              key={p.date}
              className="flex min-w-[10px] flex-1 flex-col justify-end"
              style={{ height: `${Math.max(12, h)}%` }}
              title={`${p.date}：成功 ${p.ok} · 已签 ${p.already} · 失败 ${p.failed}${
                p.successRate !== null ? `（成功率 ${p.successRate.toFixed(0)}%）` : ""
              }`}
            >
              {p.failed > 0 && (
                <div className="w-full bg-destructive/70" style={{ height: `${failPct}%` }} />
              )}
              {p.already > 0 && (
                <div className="w-full bg-muted-foreground/40" style={{ height: `${alreadyPct}%` }} />
              )}
              {p.ok > 0 && (
                <div
                  className="w-full rounded-t-sm bg-emerald-500/80"
                  style={{ height: `${okPct}%` }}
                />
              )}
            </div>
          );
        })}
      </div>
      <div className="flex items-center gap-3 text-[10px] text-muted-foreground">
        <span className="flex items-center gap-1">
          <span className="size-2 rounded-sm bg-emerald-500/80" />成功
        </span>
        <span className="flex items-center gap-1">
          <span className="size-2 rounded-sm bg-muted-foreground/40" />已签到
        </span>
        <span className="flex items-center gap-1">
          <span className="size-2 rounded-sm bg-destructive/70" />失败
        </span>
        <span className="ml-auto">
          {drawable[0]?.date} → {drawable[drawable.length - 1]?.date}
        </span>
      </div>
      <p className="flex items-start gap-1.5 text-[10px] leading-relaxed text-muted-foreground">
        <CalendarCheck className="mt-0.5 size-3 shrink-0" />
        同一天同一账号只记最终状态 —— 失败后重试成功算一次成功，不会因为重试而拉低成功率。
        没有记录的日期不画柱子（缺柱子 = 那天没跑，不是全失败）。
      </p>
    </div>
  );
}

/** 加载态（供父组件在首屏时占位）。 */
export function CheckinTrendsLoading() {
  return (
    <div className="flex items-center gap-2 py-3 text-xs text-muted-foreground">
      <Loader2 className="size-3.5 animate-spin" />
      正在读取签到趋势…
    </div>
  );
}
