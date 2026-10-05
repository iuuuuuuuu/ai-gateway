import { Activity, HeartPulse, TrendingUp } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import type { DoubaoHealthEvent } from "@/lib/api";
import { cn } from "@/lib/utils";

/** 近 14 天额度趋势（内联 SVG，不引第三方图表库）。 */
function QuotaTrend({
  trend,
}: {
  trend: { date: string; usedPercent: number }[];
}) {
  if (trend.length < 2) {
    return (
      <p className="py-6 text-center text-xs text-muted-foreground">
        额度趋势需要至少两天的数据。开启每日额度巡检后，明天起这里会出现折线。
      </p>
    );
  }

  const width = 560;
  const height = 120;
  const padX = 8;
  const padY = 10;
  // 纵轴固定 0~100：按数据自适应会把 3% 的波动放大成剧烈起伏，
  // 让人误以为额度在暴跌
  const maxY = 100;
  const stepX = (width - padX * 2) / Math.max(1, trend.length - 1);
  const points = trend.map((p, i) => {
    const x = padX + i * stepX;
    const y = height - padY - (Math.min(maxY, Math.max(0, p.usedPercent)) / maxY) * (height - padY * 2);
    return { x, y, ...p };
  });
  const line = points.map((p) => `${p.x.toFixed(1)},${p.y.toFixed(1)}`).join(" ");
  const area = `${padX},${height - padY} ${line} ${(padX + (trend.length - 1) * stepX).toFixed(1)},${height - padY}`;
  const last = points[points.length - 1];

  return (
    <div className="space-y-2">
      <svg
        viewBox={`0 0 ${width} ${height}`}
        className="h-32 w-full"
        role="img"
        aria-label={`近 ${trend.length} 天额度用量趋势`}
      >
        {/* 网格：0 / 50 / 100 三条参考线 */}
        {[0, 50, 100].map((v) => {
          const y = height - padY - (v / maxY) * (height - padY * 2);
          return (
            <g key={v}>
              <line
                x1={padX}
                x2={width - padX}
                y1={y}
                y2={y}
                className="stroke-border"
                strokeWidth="1"
                strokeDasharray={v === 0 ? undefined : "3 3"}
              />
              <text x={padX + 2} y={y - 2} className="fill-muted-foreground text-[9px]">
                {v}%
              </text>
            </g>
          );
        })}
        <polygon points={area} className="fill-primary/10" />
        <polyline
          points={line}
          fill="none"
          className="stroke-primary"
          strokeWidth="2"
          strokeLinejoin="round"
          strokeLinecap="round"
        />
        {/* 末点高亮：让「现在是多少」一眼可见 */}
        <circle cx={last.x} cy={last.y} r="3.5" className="fill-primary" />
      </svg>
      <div className="flex items-center justify-between text-[10px] text-muted-foreground">
        <span>{trend[0]?.date}</span>
        <span>
          当前时段已用 <strong className="text-foreground">{last.usedPercent.toFixed(1)}%</strong>
        </span>
        <span>{last.date}</span>
      </div>
    </div>
  );
}

/** 7 天健康卡：三类运维动作的达标情况。 */
function HealthCard({
  health,
  lastKeepaliveAt,
}: {
  health: {
    days: number;
    keepalive: number;
    renew: number;
    quota: number;
    ok: number;
    failed: number;
  };
  lastKeepaliveAt: string | null;
}) {
  const staleDays = lastKeepaliveAt ? daysSince(lastKeepaliveAt) : null;
  // 25 天阈值：豆包 sessionid 滑动续期窗口约 30 天，留 5 天余量提醒
  const stale = staleDays !== null && staleDays > 25;

  const stats = [
    { label: "保活", value: health.keepalive, icon: HeartPulse },
    { label: "续期", value: health.renew, icon: Activity },
    { label: "额度巡检", value: health.quota, icon: TrendingUp },
  ];

  return (
    <div className="space-y-3">
      <div className="grid grid-cols-3 gap-2">
        {stats.map((s) => (
          <div
            key={s.label}
            className="rounded-lg border border-border/60 px-3 py-2 text-center"
          >
            <s.icon className="mx-auto mb-1 size-3.5 text-muted-foreground" />
            <div className="text-lg font-semibold tabular-nums">{s.value}</div>
            <div className="text-[10px] text-muted-foreground">
              近 {health.days} 天{s.label}
            </div>
          </div>
        ))}
      </div>
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <Badge variant="outline" className="text-[10px]">
          成功 {health.ok}
        </Badge>
        {health.failed > 0 && (
          <Badge variant="destructive" className="text-[10px]">
            失败 {health.failed}
          </Badge>
        )}
        {lastKeepaliveAt ? (
          <span className={cn("text-muted-foreground", stale && "text-amber-600 dark:text-amber-400")}>
            最近保活：{lastKeepaliveAt.slice(0, 10)}
            {staleDays !== null && `（${staleDays} 天前）`}
          </span>
        ) : (
          <span className="text-muted-foreground">尚无保活记录</span>
        )}
      </div>
      {stale && (
        <p className="rounded-md border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs text-amber-700 dark:text-amber-300">
          已超过 25 天没有保活。豆包会话约 30 天到期，建议现在跑一次保活，避免掉登录。
        </p>
      )}
    </div>
  );
}

/** 距今多少天（解析 `YYYY-MM-DD` 或带时间的 ISO 串）。 */
function daysSince(iso: string): number | null {
  const day = iso.slice(0, 10);
  const parts = day.split("-").map(Number);
  if (parts.length !== 3 || parts.some((n) => !Number.isFinite(n))) return null;
  const then = Date.UTC(parts[0], parts[1] - 1, parts[2]);
  const now = Date.now();
  return Math.floor((now - then) / 86_400_000);
}

/**
 * 豆包运维概览：额度趋势 + 7 天健康卡 + 最近事件。
 *
 * 数据全部来自后端记录的真实运维事件（保活/续期/额度巡检），
 * 没有数据时明确说明「需要先跑一次」而不是画一条假线。
 */
export function DoubaoInsights({
  trend,
  health,
  events,
  lastKeepaliveAt,
}: {
  trend: { date: string; usedPercent: number; level: string | null }[];
  health: {
    days: number;
    keepalive: number;
    renew: number;
    quota: number;
    ok: number;
    failed: number;
  };
  events: DoubaoHealthEvent[];
  lastKeepaliveAt: string | null;
}) {
  const recent = [...events].reverse().slice(0, 8);

  return (
    <div className="grid gap-4 lg:grid-cols-2">
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">额度趋势</CardTitle>
          <CardDescription>近 14 天「当前时段」额度用量。</CardDescription>
        </CardHeader>
        <CardContent>
          <QuotaTrend trend={trend} />
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">健康概览</CardTitle>
          <CardDescription>保活 / 续期 / 额度巡检的执行情况。</CardDescription>
        </CardHeader>
        <CardContent>
          <HealthCard health={health} lastKeepaliveAt={lastKeepaliveAt} />
        </CardContent>
      </Card>

      {recent.length > 0 && (
        <Card className="lg:col-span-2">
          <CardHeader className="pb-3">
            <CardTitle className="text-base">最近运维记录</CardTitle>
            <CardDescription>保活、续期与额度巡检都会留下记录。</CardDescription>
          </CardHeader>
          <CardContent className="space-y-1.5">
            {recent.map((e, i) => (
              <div
                key={`${e.at}-${i}`}
                className="flex items-center gap-3 rounded-md border border-border/50 px-3 py-1.5 text-xs"
              >
                <Badge
                  variant={e.ok ? "outline" : "destructive"}
                  className="shrink-0 text-[10px]"
                >
                  {e.kind === "keepalive" ? "保活" : e.kind === "renew" ? "续期" : "额度"}
                </Badge>
                <span className="shrink-0 tabular-nums text-muted-foreground">
                  {e.at.replace("T", " ").slice(0, 16)}
                </span>
                <span className="min-w-0 flex-1 truncate" title={e.summary ?? ""}>
                  {e.summary ?? (e.ok ? "成功" : "失败")}
                </span>
              </div>
            ))}
          </CardContent>
        </Card>
      )}
    </div>
  );
}
