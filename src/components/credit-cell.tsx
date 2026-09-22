import { useState } from "react";
import { Coins, Loader2 } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Separator } from "@/components/ui/separator";
import * as api from "@/lib/api";
import type { TraeCreditDetail } from "@/lib/api";
import { cn } from "@/lib/utils";

/** 无到期日期的哨兵值含义（服务端用极大值表示「长期有效」）。 */
function formatExpire(expireAt: string | null | undefined): string {
  if (!expireAt) return "长期有效";
  // 2100 年后的日期都是「不限时」的哨兵，直接显示为长期有效，
  // 否则界面会出现「2126-01-01 到期」这种让人困惑的文案
  if (expireAt.startsWith("21") || expireAt.startsWith("20")) {
    const year = Number(expireAt.slice(0, 4));
    if (year >= 2100) return "长期有效";
  }
  return expireAt.slice(0, 10);
}

/**
 * 积分明细气泡：**悬停时才拉取**。
 *
 * 为什么不随列表一起返回：每个账号的明细要打三个上游接口，账号多时列表会
 * 变成几十个串行请求（数秒白屏），而绝大多数行用户根本不会看。
 */
export function CreditCell({
  userId,
  total,
  payIdentity,
}: {
  userId: string;
  total?: number | null;
  payIdentity?: string | null;
}) {
  const [open, setOpen] = useState(false);
  const [detail, setDetail] = useState<TraeCreditDetail | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = async () => {
    if (detail || loading) return;
    setLoading(true);
    setError(null);
    try {
      setDetail(await api.traeCreditDetail(userId));
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setLoading(false);
    }
  };

  return (
    <Popover
      open={open}
      onOpenChange={(o) => {
        setOpen(o);
        if (o) void load();
      }}
    >
      <PopoverTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center gap-1 rounded-md px-1.5 py-0.5 text-xs text-muted-foreground transition-colors hover:bg-accent hover:text-accent-foreground"
          aria-label="查看积分明细"
        >
          <Coins className="size-3" />
          {typeof total === "number" ? total : "积分"}
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-72 p-3 text-xs">
        <div className="mb-2 flex items-center justify-between">
          <span className="font-medium">积分明细</span>
          {payIdentity && (
            <Badge variant="outline" className="text-[10px]">
              {payIdentity}
            </Badge>
          )}
        </div>

        {loading ? (
          <div className="flex items-center gap-2 py-4 text-muted-foreground">
            <Loader2 className="size-3.5 animate-spin" />
            正在查询（需连上游，稍候）…
          </div>
        ) : error ? (
          <p className="py-2 text-destructive">{error}</p>
        ) : detail ? (
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-muted-foreground">当前可用总额</span>
              <span className="font-medium">
                {detail.total ?? "—"}
              </span>
            </div>

            {detail.packs.length > 0 && (
              <>
                <Separator />
                <div className="space-y-1.5">
                  {detail.packs.map((p, i) => (
                    <div key={`${p.name}-${i}`} className="space-y-0.5">
                      <div className="flex items-center justify-between gap-2">
                        <span className="truncate" title={p.name}>
                          {p.name}
                        </span>
                        <span className="shrink-0 font-medium">
                          {p.remaining}
                          {typeof p.total === "number" && p.total > 0 && (
                            <span className="text-muted-foreground"> / {p.total}</span>
                          )}
                        </span>
                      </div>
                      <div className="text-[10px] text-muted-foreground">
                        {p.expireAt ? `${formatExpire(p.expireAt)} 到期` : "长期有效"}
                      </div>
                    </div>
                  ))}
                </div>
              </>
            )}

            {detail.payExpireAt && (
              <>
                <Separator />
                <div className="flex items-center justify-between">
                  <span className="text-muted-foreground">套餐到期</span>
                  <span>{formatExpire(detail.payExpireAt)}</span>
                </div>
              </>
            )}

            {/*
              部分失败要如实说明：三个接口里有一个挂了，界面必须告诉用户
              「这里的数据不全」，否则用户会以为积分真的只剩这么点。
            */}
            {detail.errors.length > 0 && (
              <>
                <Separator />
                <div className="space-y-0.5 text-[10px] text-amber-600 dark:text-amber-400">
                  <div>部分数据未能获取：</div>
                  {detail.errors.map((e, i) => (
                    <div key={i} className="break-all">
                      · {e}
                    </div>
                  ))}
                </div>
              </>
            )}
          </div>
        ) : (
          <p className="py-2 text-muted-foreground">暂无数据</p>
        )}
      </PopoverContent>
    </Popover>
  );
}

/** JWT 剩余有效期的展示文案（`expHours` 为负表示已过期）。 */
export function formatJwtHours(hours: number | null | undefined): string {
  if (hours === null || hours === undefined) return "未知";
  if (hours < 0) return "已过期";
  if (hours < 1) return "不足 1 小时";
  if (hours < 48) return `${Math.round(hours)} 小时`;
  return `${Math.round(hours / 24)} 天`;
}

/** 套餐身份徽标的语义色。 */
export function payIdentityClass(identity: string | null | undefined): string {
  if (!identity) return "border-border bg-muted text-muted-foreground";
  const v = identity.toLowerCase();
  // 付费档位越高越醒目：免费档保持中性，避免整屏都是彩色徽标
  if (v.includes("max") || v.includes("pro"))
    return "border-violet-500/40 bg-violet-500/10 text-violet-700 dark:text-violet-300";
  if (v.includes("plus") || v.includes("team"))
    return "border-sky-500/40 bg-sky-500/10 text-sky-700 dark:text-sky-300";
  return cn("border-border bg-muted text-muted-foreground");
}
