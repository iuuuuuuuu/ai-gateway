import { useCallback, useEffect, useState } from "react";
import { AlertTriangle, Coins, Loader2, RefreshCw, TrendingDown } from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Separator } from "@/components/ui/separator";
import { Skeleton } from "@/components/ui/skeleton";
import * as api from "@/lib/api";
import { cn } from "@/lib/utils";

/** 区间选择（与参考实现的「今日/近7天/近30天」对齐）。 */
const RANGES = [
  { days: 7, label: "近 7 天" },
  { days: 30, label: "近 30 天" },
  { days: 90, label: "近 90 天" },
];

/** 数字格式化（千分位）。 */
function fmt(n: number | null | undefined): string {
  if (n === null || n === undefined) return "—";
  return n.toLocaleString("zh-CN");
}

/** 积分保留两位（官方返回的是浮点）。 */
function fmtCredits(n: number): string {
  if (n === 0) return "0";
  if (Math.abs(n) >= 100) return n.toFixed(0);
  return n.toFixed(2).replace(/\.?0+$/, "");
}

/**
 * 官方消耗：真实扣费 + 模型排行。
 *
 * 与上面的「余额差值」口径并排展示，因为两者回答不同问题：
 * 差值法看的是**余额净变化**（含签到补发），官方接口给的是**真实扣费**。
 * 只留一个都会让人误判 —— 只看差值会以为「今天没消耗」，实际是签到补平了。
 */
function OfficialUsage() {
  const [data, setData] = useState<api.TraeUsageHistory | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);

  const load = useCallback(
    async (fresh: boolean) => {
      if (fresh) setBusy(true);
      else setLoading(true);
      try {
        setData(await api.traeUsageHistory(fresh));
      } catch (e) {
        toast.error(api.asError(e));
      } finally {
        setLoading(false);
        setBusy(false);
      }
    },
    [],
  );

  useEffect(() => {
    // 首屏读缓存：进页面就打一次上游会明显拖慢加载
    void load(false);
  }, [load]);

  if (loading) return <Skeleton className="h-32 w-full" />;
  if (!data) return null;

  const withData = data.accounts.filter((a) => a.daily.length > 0);
  const failed = data.accounts.filter((a) => !a.ok);
  const totalCredits = withData.reduce((s, a) => s + a.totalCredits, 0);
  const totalSessions = withData.reduce((s, a) => s + a.sessions, 0);

  // 跨账号合并模型排行
  const merged = new Map<string, number>();
  for (const a of withData) {
    for (const m of a.models) merged.set(m.model, (merged.get(m.model) ?? 0) + m.credits);
  }
  const ranking = [...merged.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8);
  const maxModel = Math.max(0.0001, ...ranking.map((r) => r[1]));

  // 近 14 天按模型堆叠的日消耗
  const byDate = new Map<string, Record<string, number>>();
  for (const a of withData) {
    for (const d of a.daily) {
      const slot = byDate.get(d.date) ?? {};
      for (const [m, c] of Object.entries(d.models)) slot[m] = (slot[m] ?? 0) + c;
      byDate.set(d.date, slot);
    }
  }
  const dates = [...byDate.keys()].sort().slice(-14);

  return (
    <Card>
      <CardHeader className="pb-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div>
            <CardTitle className="text-base">官方消耗明细</CardTitle>
            <CardDescription>
              直连官方会话级用量接口，是<strong className="font-medium text-foreground">真实扣费</strong>
              ；签到补发不会计入。
            </CardDescription>
          </div>
          <Button size="sm" variant="outline" disabled={busy} onClick={() => void load(true)}>
            {busy ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
            刷新
          </Button>
        </div>
      </CardHeader>
      <CardContent className="space-y-4">
        {withData.length === 0 ? (
          <p className="py-4 text-center text-xs text-muted-foreground">
            还没有消耗记录。点「刷新」从官方拉取。
          </p>
        ) : (
          <>
            <div className="flex flex-wrap gap-4 text-xs">
              <div>
                <div className="text-[10px] text-muted-foreground">区间总消耗</div>
                <div className="text-lg font-semibold tabular-nums">
                  {fmtCredits(totalCredits)}
                </div>
              </div>
              <div>
                <div className="text-[10px] text-muted-foreground">会话数</div>
                <div className="text-lg font-semibold tabular-nums">{fmt(totalSessions)}</div>
              </div>
              <div>
                <div className="text-[10px] text-muted-foreground">覆盖账号</div>
                <div className="text-lg font-semibold tabular-nums">{withData.length}</div>
              </div>
            </div>

            <Separator />

            <div className="space-y-1.5">
              <div className="text-xs font-medium">模型消耗排行</div>
              {ranking.map(([model, credits]) => (
                <div key={model} className="flex items-center gap-2 text-xs">
                  <span className="w-40 truncate font-mono" title={model}>
                    {model}
                  </span>
                  <div className="h-2 flex-1 overflow-hidden rounded-full bg-muted">
                    <div
                      className="h-full rounded-full bg-primary/70"
                      style={{ width: `${(credits / maxModel) * 100}%` }}
                    />
                  </div>
                  <span className="w-16 text-right tabular-nums">{fmtCredits(credits)}</span>
                </div>
              ))}
            </div>

            {dates.length > 0 && (
              <>
                <Separator />
                <div className="space-y-1.5">
                  <div className="text-xs font-medium">近 {dates.length} 天扣费</div>
                  <div className="flex h-16 items-end gap-1">
                    {(() => {
                      const totals = dates.map((d) =>
                        Object.values(byDate.get(d) ?? {}).reduce((s, v) => s + v, 0),
                      );
                      const max = Math.max(0.0001, ...totals);
                      return dates.map((d, i) => (
                        <div
                          key={d}
                          className="min-w-[8px] flex-1 rounded-t-sm bg-primary/70"
                          style={{ height: `${Math.max(6, (totals[i] / max) * 100)}%` }}
                          title={`${d}：${fmtCredits(totals[i])} 积分`}
                        />
                      ));
                    })()}
                  </div>
                  <div className="flex justify-between text-[10px] text-muted-foreground">
                    <span>{dates[0]}</span>
                    <span>{dates[dates.length - 1]}</span>
                  </div>
                </div>
              </>
            )}
          </>
        )}

        {failed.length > 0 && (
          <Alert>
            <AlertTriangle className="size-4" />
            <AlertTitle>{failed.length} 个账号未能拉取</AlertTitle>
            <AlertDescription>
              <ul className="mt-1 space-y-0.5 text-xs">
                {failed.map((a) => (
                  <li key={a.userId}>
                    <span className="font-medium">{a.name || a.userId}</span>
                    {a.error && <span className="text-muted-foreground">：{a.error}</span>}
                  </li>
                ))}
              </ul>
            </AlertDescription>
          </Alert>
        )}
      </CardContent>
    </Card>
  );
}

/**
 * 积分趋势折线（内联 SVG，不引第三方图表库）。
 *
 * 缺天（`total === null`）**断线**而不是连过去：把相隔一周的两个点直连会
 * 画出一条看似平缓的下降线，实际那一周根本没采到数据，用户会据此误判消耗速度。
 */
function CreditsChart({ daily }: { daily: api.TraeCreditsDay[] }) {
  const points = daily.filter((d) => d.total !== null);
  if (points.length < 2) {
    return (
      <div className="py-8 text-center text-xs text-muted-foreground">
        {points.length === 0
          ? "还没有积分快照。点「立即采样」或等待每日快照任务运行后，这里会出现趋势。"
          : "只有一天的快照，暂时画不出趋势。明天再采样一次即可。"}
      </div>
    );
  }

  const width = 620;
  const height = 140;
  const padX = 10;
  const padY = 12;
  const values = points.map((p) => p.total as number);
  const maxV = Math.max(...values);
  const minV = Math.min(...values);
  // 上下各留 8% 余量：贴着边框的折线看不出是涨还是跌
  const span = Math.max(1, maxV - minV);
  const lo = minV - span * 0.08;
  const hi = maxV + span * 0.08;
  const stepX = (width - padX * 2) / Math.max(1, daily.length - 1);
  const yOf = (v: number) => height - padY - ((v - lo) / (hi - lo)) * (height - padY * 2);

  // 按「连续有数据的段」切分，缺天处断开
  const segments: { x: number; y: number }[][] = [];
  let current: { x: number; y: number }[] = [];
  daily.forEach((d, i) => {
    if (d.total === null) {
      if (current.length > 0) segments.push(current);
      current = [];
      return;
    }
    current.push({ x: padX + i * stepX, y: yOf(d.total) });
  });
  if (current.length > 0) segments.push(current);

  const last = points[points.length - 1];

  return (
    <div className="space-y-2">
      <svg
        viewBox={`0 0 ${width} ${height}`}
        className="h-36 w-full"
        role="img"
        aria-label="积分余额趋势"
      >
        {/* 上/下界参考线 */}
        {[lo, hi].map((v, i) => (
          <g key={i}>
            <line
              x1={padX}
              x2={width - padX}
              y1={yOf(v)}
              y2={yOf(v)}
              className="stroke-border"
              strokeWidth="1"
              strokeDasharray="3 3"
            />
            <text x={padX + 2} y={yOf(v) - 3} className="fill-muted-foreground text-[9px]">
              {Math.round(v)}
            </text>
          </g>
        ))}
        {segments.map((seg, i) => (
          <g key={i}>
            {seg.length > 1 && (
              <polyline
                points={seg.map((p) => `${p.x.toFixed(1)},${p.y.toFixed(1)}`).join(" ")}
                fill="none"
                className="stroke-primary"
                strokeWidth="2"
                strokeLinejoin="round"
                strokeLinecap="round"
              />
            )}
            {seg.map((p, j) => (
              <circle key={j} cx={p.x} cy={p.y} r="2.5" className="fill-primary" />
            ))}
          </g>
        ))}
      </svg>
      <div className="flex items-center justify-between text-[10px] text-muted-foreground">
        <span>{daily[0]?.date}</span>
        <span>
          最新 <strong className="text-foreground">{fmt(last.total)}</strong> 积分
        </span>
        <span>{daily[daily.length - 1]?.date}</span>
      </div>
    </div>
  );
}

/**
 * Trae 积分总览：趋势 + 消耗汇总 + 各账号余额。
 *
 * 数据全部来自本地积分快照（`trae_credits_history.json`）。界面上如实标注
 * 「实际采到几天数据」—— 只有 2 天数据却画 30 天坐标轴，会让人以为趋势很可靠。
 */
export function TraeCreditsPanel() {
  const [stats, setStats] = useState<api.TraeCreditsStats | null>(null);
  const [days, setDays] = useState(30);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setStats(await api.traeCreditsStats(days));
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, [days]);

  useEffect(() => {
    void load();
  }, [load]);

  const sampleNow = async () => {
    setBusy(true);
    try {
      const res = await api.traeCreditsSnapshot();
      if (res.failed > 0) {
        toast.warning(`采样 ${res.sampled} 个成功、${res.failed} 个失败`);
      } else {
        toast.success(`已采样 ${res.sampled} 个账号的积分`);
      }
      await load();
    } catch (e) {
      toast.error(api.asError(e));
    } finally {
      setBusy(false);
    }
  };

  const s = stats?.summary;
  // 数据稀疏时明确提示：两天数据画不出可信趋势
  const sparse = (s?.observedDays ?? 0) < Math.min(3, days);

  return (
    <div className="space-y-4">
      <div className="grid gap-4 sm:grid-cols-3">
        <Card>
          <CardHeader className="pb-2">
            <CardDescription className="flex items-center gap-1.5">
              <Coins className="size-3.5" />
              当前总积分
            </CardDescription>
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-semibold tabular-nums">
              {loading ? <Skeleton className="h-7 w-24" /> : fmt(s?.latestTotal)}
            </div>
            {typeof s?.change === "number" && (
              <p
                className={cn(
                  "mt-1 flex items-center gap-1 text-xs",
                  s.change < 0 ? "text-destructive" : "text-muted-foreground",
                )}
              >
                <TrendingDown className="size-3" />
                区间内 {s.change > 0 ? "+" : ""}
                {fmt(s.change)}
              </p>
            )}
            {s && s.change === null && (s.observedDays ?? 0) > 0 && (
              <p className="mt-1 text-xs text-muted-foreground">数据不足，暂时算不出变化量</p>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardDescription>区间消耗</CardDescription>
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-semibold tabular-nums">
              {loading ? <Skeleton className="h-7 w-24" /> : fmt(s?.consumed)}
            </div>
            <p className="mt-1 text-xs text-muted-foreground">
              只统计有快照的天，充值不计入
            </p>
          </CardContent>
        </Card>

        <Card>
          <CardHeader className="pb-2">
            <CardDescription>有效数据</CardDescription>
          </CardHeader>
          <CardContent>
            <div className="text-2xl font-semibold tabular-nums">
              {loading ? (
                <Skeleton className="h-7 w-16" />
              ) : (
                <>
                  {s?.observedDays ?? 0}
                  <span className="text-sm font-normal text-muted-foreground"> / {days} 天</span>
                </>
              )}
            </div>
            <p className="mt-1 text-xs text-muted-foreground">
              {sparse ? "数据偏少，趋势仅供参考" : "数据充足"}
            </p>
          </CardContent>
        </Card>
      </div>

      <Card>
        <CardHeader className="pb-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <CardTitle className="text-base">积分趋势</CardTitle>
              <CardDescription>
                缺数据的天会断线显示 —— 断开处表示那天没有快照，不是积分归零。
              </CardDescription>
            </div>
            <div className="flex items-center gap-1.5">
              <div className="flex gap-1 rounded-lg bg-muted p-0.5" role="tablist" aria-label="统计区间">
                {RANGES.map((r) => (
                  <button
                    key={r.days}
                    type="button"
                    role="tab"
                    aria-selected={days === r.days}
                    onClick={() => setDays(r.days)}
                    className={cn(
                      "rounded-md px-2 py-1 text-xs transition-colors",
                      days === r.days
                        ? "bg-background text-foreground shadow-sm"
                        : "text-muted-foreground hover:text-foreground",
                    )}
                  >
                    {r.label}
                  </button>
                ))}
              </div>
              <Button size="sm" variant="outline" disabled={busy} onClick={() => void sampleNow()}>
                {busy ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
                立即采样
              </Button>
            </div>
          </div>
        </CardHeader>
        <CardContent>
          {loading ? (
            <Skeleton className="h-36 w-full" />
          ) : stats ? (
            <CreditsChart daily={stats.daily} />
          ) : null}
        </CardContent>
      </Card>

      {stats && stats.accounts.length > 0 && (
        <Card>
          <CardHeader className="pb-3">
            <CardTitle className="text-base">各账号余额</CardTitle>
            <CardDescription>最近一次采到的积分总量。</CardDescription>
          </CardHeader>
          <CardContent className="space-y-1.5">
            {stats.accounts.map((a) => (
              <div
                key={a.userId}
                className="flex flex-wrap items-center gap-2 rounded-md border border-border/50 px-3 py-2 text-xs"
              >
                <span className="font-medium">{a.name || "（无备注名）"}</span>
                <span className="truncate font-mono text-muted-foreground">{a.userId}</span>
                <span className="ml-auto tabular-nums">
                  {a.credits === null ? (
                    <Badge variant="outline" className="text-[10px]">
                      未采样
                    </Badge>
                  ) : (
                    `${fmt(a.credits)} 积分`
                  )}
                </span>
                {a.updatedAt && (
                  <span className="text-[10px] text-muted-foreground">{a.updatedAt.slice(0, 16)}</span>
                )}
              </div>
            ))}
          </CardContent>
        </Card>
      )}

      {stats && stats.summary.observedDays === 0 && (
        <Alert>
          <Coins className="size-4" />
          <AlertTitle>还没有积分快照</AlertTitle>
          <AlertDescription>
            点右上角「立即采样」即可立刻记录一次；也可以注册每日「积分快照」计划任务
            让它自动累积。首次采样后统计才会开始累计。
          </AlertDescription>
        </Alert>
      )}

      {/* 官方口径：与上面的余额差值口径并排，两个都要看 */}
      <OfficialUsage />
    </div>
  );
}
